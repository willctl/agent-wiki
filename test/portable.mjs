// What differs between Windows, Linux and macOS in the runtime: lock liveness, how codex is started
// and stopped, shell quoting of the hook command, case-sensitive file names, symlinked paths.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { after, describe, test } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { codexCommand, killTree } from '../src/codex.mjs';
import { acquireLock, machineId, ownerLiveness, PROCESS_START, processStartMs, releaseAlive } from '../src/lock.mjs';
import { shellToken } from '../scripts/lib.mjs';
import { IMPL, rustBin, srvT } from './impl.mjs';

const REPO = fileURLToPath(new URL('..', import.meta.url));
const WIN = process.platform === 'win32';
const ROOT = typeof process.getuid === 'function' && process.getuid() === 0;
const tmp = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-portable-'));
after(async () => {
  releaseAlive();
  await fsp.chmod(path.join(tmp, 'ro', '.locks'), 0o755).catch(() => {});
  await fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {});
});
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code !== 'ESRCH';
  }
};

describe('lock owners', () => {
  test('a pid that another process reuses reads as dead (POSIX: start time); this process reads as alive', { skip: WIN && 'Windows checks owners through alive files' }, () => {
    const started = processStartMs(process.pid);
    assert.ok(started, 'the start time of this process is readable');
    assert.ok(Math.abs(started - PROCESS_START) < 2000, `start ${started} vs recorded ${PROCESS_START}`);
    const locks = path.join(tmp, 'locks-a');
    const me = { pid: process.pid, start: PROCESS_START, host: os.hostname(), machine: machineId() };
    assert.equal(ownerLiveness(locks, me), 'alive');
    // A child's pid with a start time from an hour ago: the owner that recorded it is gone.
    const child = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 30000)'], { stdio: 'ignore' });
    try {
      const fake = { pid: child.pid, start: PROCESS_START - 3_600_000, host: os.hostname(), machine: machineId() };
      assert.equal(ownerLiveness(locks, fake), 'dead');
      const real = { ...fake, start: processStartMs(child.pid) };
      assert.equal(ownerLiveness(locks, real), 'alive');
    } finally {
      child.kill();
    }
  });

  test('an owner from another machine is "unknown"; the machine id is stable', () => {
    assert.ok(machineId());
    assert.equal(machineId(), machineId());
    assert.equal(ownerLiveness(path.join(tmp, 'locks-b'), { pid: 1, start: 1, host: 'elsewhere', machine: 'another-machine' }), 'unknown');
  });

  test('no permission on .locks: acquiring gives up in time instead of spinning', { skip: (WIN || ROOT) && 'needs POSIX permissions (and not root)' }, async () => {
    const wiki = path.join(tmp, 'ro');
    await fsp.mkdir(path.join(wiki, '.locks'), { recursive: true });
    await fsp.chmod(path.join(wiki, '.locks'), 0o555);
    const t0 = Date.now();
    await assert.rejects(acquireLock(wiki, 'write', { timeoutMs: 300 }), (e) => e.code === 'EACCES');
    assert.ok(Date.now() - t0 < 5000, 'returned promptly');
  });
});

describe('starting and stopping codex', () => {
  test('a .js/.mjs entry runs with this Node; an extensionless #!/usr/bin/env node script too (npm on POSIX)', async () => {
    const mjs = path.join(REPO, 'test', 'fixtures', 'fake-codex.mjs');
    assert.deepEqual(codexCommand(mjs), [process.execPath, fs.realpathSync(mjs)]);
    assert.deepEqual(codexCommand('codex'), ['codex'], 'a bare command runs as given');
    if (WIN) return;
    const dir = path.join(tmp, 'npm-bin');
    await fsp.mkdir(path.join(dir, 'lib'), { recursive: true });
    const script = path.join(dir, 'lib', 'codex.js-without-extension');
    await fsp.writeFile(script, '#!/usr/bin/env node\nconsole.log("hi")\n', { mode: 0o755 });
    const link = path.join(dir, 'codex');
    await fsp.symlink(script, link);
    assert.deepEqual(codexCommand(link), [process.execPath, fs.realpathSync(script)]);
    const bin = path.join(dir, 'native');
    await fsp.writeFile(bin, '\x7fELF', { mode: 0o755 });
    assert.deepEqual(codexCommand(bin), [bin], 'a native binary runs as itself');
  });

  test('killTree stops codex and what it started', async () => {
    // A child that starts a grandchild and reports its pid, like codex starting the read-only MCP server.
    const code = `const { spawn } = require('child_process'); const g = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 60000)'], { stdio: 'ignore' }); console.log(g.pid); setTimeout(() => {}, 60000);`;
    const child = spawn(process.execPath, ['-e', code], { stdio: ['ignore', 'pipe', 'ignore'], detached: !WIN, windowsHide: true });
    const grandchild = await new Promise((resolve) => child.stdout.once('data', (d) => resolve(Number(String(d).trim()))));
    assert.ok(alive(grandchild));
    killTree(child);
    const deadline = Date.now() + 10_000;
    while ((alive(grandchild) || alive(child.pid)) && Date.now() < deadline) await sleep(100);
    assert.ok(!alive(grandchild), 'the grandchild is gone');
    assert.ok(!alive(child.pid), 'the child is gone');
  });
});

describe('hook command', () => {
  test('POSIX: paths with spaces and quotes stay one token', { skip: WIN && 'Windows uses 8.3 names (installer tests)' }, async () => {
    assert.equal(shellToken('/usr/local/bin/node', 'linux'), '/usr/local/bin/node');
    const odd = path.join(tmp, "Application Support", "it's here", 'echo.mjs');
    await fsp.mkdir(path.dirname(odd), { recursive: true });
    await fsp.writeFile(odd, 'console.log(JSON.stringify(process.argv.slice(1)))\n');
    const cmd = `${shellToken(process.execPath)} ${shellToken(odd)} done`;
    for (const sh of ['/bin/sh', '/bin/bash', '/bin/zsh'].filter((s) => fs.existsSync(s))) {
      const r = spawnSync(sh, ['-c', cmd], { encoding: 'utf8' });
      assert.equal(r.status, 0, `${sh}: ${r.stderr}`);
      assert.deepEqual(JSON.parse(r.stdout), [path.resolve(odd), 'done'], sh); // argv[1] is resolved, not realpath'd
    }
  });
});

describe('files and paths', () => {
  test('wiki_read finds INDEX.md and Agent-Wiki on a case-sensitive filesystem', async () => {
    const wikiDir = path.join(tmp, 'wiki-case');
    const env = { ...process.env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: path.join(tmp, 'home-case'), AGENT_WIKI_WRITE_MODE: 'direct' };
    const client = new Client({ name: 'portable-test', version: '1' });
    await client.connect(new StdioClientTransport({ ...srvT(path.join(REPO, 'dist', 'runtime')), env, stderr: 'ignore' }));
    try {
      const w = await client.callTool({ name: 'wiki_upsert_page', arguments: { app: 'test', title: 'Agent Wiki', type: 'project', summary: 'test page', content: '# Agent Wiki\n\nHello.', mode: 'replace' } });
      assert.ok(!w.isError, w.content?.[0]?.text);
      for (const target of ['INDEX.md', 'Agent-Wiki', 'protocol.md']) {
        const r = await client.callTool({ name: 'wiki_read', arguments: { target } });
        assert.ok(!r.isError, `${target}: ${r.content?.[0]?.text}`);
      }
    } finally {
      await client.close();
    }
  });

  test('the curator starts from a symlinked folder, with an npm-style codex (node beside it, as npm installs them) and no node on PATH', { skip: WIN && 'symlinks need privileges on Windows' }, async () => {
    // macOS: os.tmpdir() is under /var, a symlink to /private/var; Node resolves the entry module's real
    // path but not argv[1], and a plain comparison of the two made main() silently never run.
    const real = path.join(tmp, 'real-runtime');
    await fsp.cp(path.join(REPO, 'dist', 'runtime'), real, { recursive: true });
    if (IMPL === 'rust') await fsp.copyFile(rustBin(), path.join(real, 'agent-wiki'));
    const link = path.join(tmp, 'linked-runtime');
    await fsp.symlink(real, link, 'dir');
    const npmBin = path.join(tmp, 'npm-prefix-bin');
    await fsp.mkdir(npmBin);
    await fsp.symlink(process.execPath, path.join(npmBin, 'node'));
    const codex = path.join(npmBin, 'codex');
    const fake = pathToFileURL(path.join(REPO, 'test', 'fixtures', 'fake-codex.mjs')).href;
    await fsp.writeFile(codex, `#!/usr/bin/env node\nimport(${JSON.stringify(fake)});\n`, { mode: 0o755 });
    const home = path.join(tmp, 'home-link');
    await fsp.mkdir(home, { recursive: true });
    await fsp.writeFile(path.join(home, 'config.json'), JSON.stringify({ curator: { codexPath: codex, codexHome: path.join(home, 'codex-home') } }));
    const [cmd, args] = IMPL === 'rust' ? [path.join(link, 'agent-wiki'), ['curator', '--login-status']] : [process.execPath, [path.join(link, 'curator.mjs'), '--login-status']];
    const r = spawnSync(cmd, args, {
      encoding: 'utf8',
      env: { HOME: home, AGENT_WIKI_HOME: home, AGENT_WIKI_DIR: path.join(tmp, 'wiki-link'), PATH: '/nonexistent' },
      timeout: 30_000,
    });
    assert.equal(r.status, 0, r.stderr);
    assert.deepEqual(JSON.parse(r.stdout.trim().split('\n').pop()).signedIn, true, r.stdout + r.stderr);
  });
});
