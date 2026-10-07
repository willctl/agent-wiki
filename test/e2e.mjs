// End-to-end tests: two real MCP clients (standing in for Claude and ChatGPT)
// talk to the BUNDLED server at the same time, against a temp wiki, once over
// stdio (each app launches its own server) and once over Streamable HTTP (the
// Windows service). The runtime is copied out of the repo first, which proves
// it needs no node_modules.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { hookCmd, IMPL, rustBin, srv, srvT } from './impl.mjs';

const repo = fileURLToPath(new URL('..', import.meta.url));
const dist = path.join(repo, 'dist', 'runtime');
if (!fs.existsSync(path.join(dist, 'server.mjs'))) throw new Error('Run `npm run build` first.');

const tmp = await fsp.mkdtemp(path.join(os.tmpdir(), 'agent-wiki-e2e-'));
const runtime = path.join(tmp, 'runtime');
await fsp.cp(dist, runtime, { recursive: true });
const bundledProtocol = fs.readFileSync(path.join(repo, 'protocol', 'PROTOCOL.md'), 'utf8');
const today = (() => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
})();

after(() => fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {}));

/** Lock directories left in .locks/ (alive/ holds each running process's liveness file, not a lock). */
const leftoverLocks = (wikiDir) => fs.readdirSync(path.join(wikiDir, '.locks')).filter((n) => n !== 'alive');

async function call(client, name, args) {
  const r = await client.callTool({ name, arguments: args });
  return { text: r.content.map((c) => c.text).join('\n'), isError: Boolean(r.isError) };
}

async function ok(client, name, args) {
  const r = await call(client, name, args);
  assert.equal(r.isError, false, `${name} failed: ${r.text}`);
  return r.text;
}

/** Starts `server.mjs --http --port 0 --parent-stdin`; resolves with {proc, url, port}. */
function startHttpServer(env) {
  return new Promise((resolve, reject) => {
    const proc = spawn(...srv(runtime, '--http', '--port', '0', '--parent-stdin'), {
      env,
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    let err = '';
    const timer = setTimeout(() => reject(new Error(`server did not start:\n${err}`)), 10_000);
    proc.stderr.setEncoding('utf8');
    proc.stderr.on('data', (d) => {
      err += d;
      const m = err.match(/listening on (http:\/\/127\.0\.0\.1:(\d+)\/mcp)/);
      if (m) {
        clearTimeout(timer);
        resolve({ proc, url: m[1], port: Number(m[2]) });
      }
    });
    proc.on('exit', (code) => reject(new Error(`server exited ${code}:\n${err}`)));
  });
}

function rawRequest(port, { method = 'GET', path: p = '/health', headers = {}, body } = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, method, path: p, headers }, (res) => {
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (data += d));
      res.on('end', () => resolve({ status: res.statusCode, body: data }));
    });
    req.on('error', reject);
    if (body) req.write(body);
    req.end();
  });
}

for (const mode of ['stdio', 'http']) {
  describe(`${mode} transport`, () => {
    const wikiDir = path.join(tmp, `wiki-${mode}`);
    // The v1.1 suite exercises direct writes; curated (queued) writes are covered in test/curator.mjs.
    const env = { ...process.env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: path.join(tmp, 'home'), AGENT_WIKI_WRITE_MODE: 'direct' };
    const todayLog = path.join(wikiDir, 'log', today.slice(0, 4), `${today}.md`);
    let claude;
    let chatgpt;
    let httpServer;

    async function connect(name) {
      const transport =
        mode === 'stdio'
          ? new StdioClientTransport({ ...srvT(runtime), env, cwd: tmp, stderr: 'pipe' })
          : new StreamableHTTPClientTransport(new URL(httpServer.url));
      const client = new Client({ name, version: '1.0.0' });
      await client.connect(transport);
      return client;
    }

    before(async () => {
      if (mode === 'http') httpServer = await startHttpServer(env);
      // Both "apps" start at once, so skeleton creation races too.
      [claude, chatgpt] = await Promise.all([connect('claude-desktop'), connect('chatgpt-desktop')]);
    });

    after(async () => {
      await Promise.allSettled([claude?.close(), chatgpt?.close()]);
      if (httpServer && httpServer.proc.exitCode === null) httpServer.proc.kill();
    });

    test('server instructions include the lead-in and the protocol', () => {
      for (const c of [claude, chatgpt]) {
        const ins = c.getInstructions();
        assert.match(ins, /Call wiki_start once at the beginning of EVERY conversation/);
        assert.ok(ins.includes(bundledProtocol.trim()), 'instructions contain PROTOCOL.md');
        assert.ok(ins.includes(wikiDir.replace(/\\/g, '/')), 'instructions name the wiki folder');
      }
    });

    test('exactly the 5 tools exist, with readOnly annotations', async () => {
      const { tools } = await claude.listTools();
      const byName = Object.fromEntries(tools.map((t) => [t.name, t]));
      assert.deepEqual(Object.keys(byName).sort(), ['wiki_log', 'wiki_read', 'wiki_search', 'wiki_start', 'wiki_upsert_page']);
      for (const n of ['wiki_start', 'wiki_search', 'wiki_read']) assert.equal(byName[n].annotations?.readOnlyHint, true, n);
      for (const n of ['wiki_log', 'wiki_upsert_page']) assert.equal(byName[n].annotations?.readOnlyHint, false, n);
      assert.match(byName.wiki_start.description, /^ALWAYS call this once at the start of every conversation/);
    });

    test('the wiki skeleton is created', () => {
      for (const f of ['PROTOCOL.md', 'README.md', 'index.md', '.gitattributes', '.gitignore']) {
        assert.ok(fs.statSync(path.join(wikiDir, f)).isFile(), f);
      }
      for (const d of ['pages', 'log', '.history', '.locks']) assert.ok(fs.statSync(path.join(wikiDir, d)).isDirectory(), d);
      assert.equal(fs.readFileSync(path.join(wikiDir, 'PROTOCOL.md'), 'utf8'), bundledProtocol);
      assert.equal(fs.readFileSync(path.join(wikiDir, '.gitattributes'), 'utf8'), '* text=auto eol=lf\n');
      assert.match(fs.readFileSync(path.join(wikiDir, '.gitignore'), 'utf8'), /^\.locks\/$/m);
      assert.match(fs.readFileSync(path.join(wikiDir, 'index.md'), 'utf8'), /GENERATED.*Do not edit/);
    });

    test('wiki_start works on an empty wiki', async () => {
      const text = await ok(claude, 'wiki_start', { app: 'claude-desktop', topic: 'anything' });
      assert.match(text, /\(no pages yet\)/);
      assert.match(text, /# Agent Wiki protocol/);
      assert.match(text, /Now: \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}/);
      assert.match(text, /\(no activity logged yet\)/);
    });

    test('page creation writes frontmatter and LF only, even from CRLF input', async () => {
      await ok(claude, 'wiki_upsert_page', {
        app: 'Claude Desktop',
        title: 'Atlas',
        slug: 'atlas',
        type: 'project',
        summary: 'Audit app: Docker + Supabase, dev on Staging',
        tags: ['atlas', 'Docker'],
        content: 'Line one\r\nLine two\r\n\r\n## Setup\r\nRuns in docker compose.\r\n',
      });
      const raw = fs.readFileSync(path.join(wikiDir, 'pages', 'atlas.md'), 'utf8');
      assert.ok(!raw.includes('\r'), 'no CR in page');
      const fm = raw.split('\n---\n')[0];
      assert.match(fm, /^---\ntitle: Atlas\ntype: project\n/);
      assert.match(fm, /^summary: "Audit app: Docker \+ Supabase, dev on Staging"$/m);
      assert.match(fm, /^tags: \["atlas","docker"\]$/m);
      assert.match(fm, /^created: \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}$/m);
      assert.match(fm, /^updated: \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}[+-]\d{2}:\d{2}$/m);
      assert.match(fm, /^updated_by: claude-desktop$/m);
      assert.match(raw, /\n---\n\n# Atlas\n\nLine one\nLine two\n/);
      const index = fs.readFileSync(path.join(wikiDir, 'index.md'), 'utf8');
      assert.match(index, /## Projects\n\n- \[\[atlas\|Atlas\]\] - Audit app: Docker \+ Supabase, dev on Staging/);
      assert.ok(!index.includes('\r'));
      const log = fs.readFileSync(todayLog, 'utf8');
      assert.match(log, /^- \d{2}:\d{2} · claude-desktop · Page created: Atlas \[\[atlas\]\]$/m);
      assert.ok(!log.includes('\r'));
    });

    test('a log written by the other client shows up in search', async () => {
      const r = await ok(chatgpt, 'wiki_log', {
        app: 'chatgpt-desktop',
        title: 'Fixed Atlas docker networking',
        body: 'Containers now share the atlas_net bridge network.\r\nRestarted with docker compose up -d.',
        tags: ['atlas', 'docker'],
        pages: ['atlas'],
      });
      assert.match(r, /Logged to log\/\d{4}\/\d{4}-\d{2}-\d{2}\.md/);
      const log = fs.readFileSync(todayLog, 'utf8');
      assert.match(log, /^## \d{2}:\d{2} · chatgpt-desktop · Fixed Atlas docker networking\n\ntags: atlas, docker {2}\npages: \[\[atlas\]\]\n\nContainers now share/m);
      const hits = await ok(claude, 'wiki_search', { query: 'bridge network' });
      assert.ok(hits.includes(`log/${today.slice(0, 4)}/${today}.md`), hits);
      assert.match(hits, /> \[\d{2}:\d{2} · chatgpt-desktop · Fixed Atlas docker networking\] Containers now share the atlas_net bridge network/);
      const pagesOnly = await ok(claude, 'wiki_search', { query: 'docker', scope: 'pages' });
      assert.match(pagesOnly, /pages\/atlas\.md/);
      assert.ok(!pagesOnly.includes('log/'), pagesOnly);
      assert.match(await ok(claude, 'wiki_search', { query: 'zebra unicorn' }), /No results/);
    });

    test('search cites the section that matched; wiki_read opens just that section; date and app filters', { skip: IMPL !== 'rust' && 'the Rust programs only' }, async () => {
      // M5: a log entry is a section of its day, read as "<date>#<anchor>".
      const hits = await ok(claude, 'wiki_search', { query: 'atlas_net bridge' });
      const m = hits.match(new RegExp(`\\(read: "(${today}#[a-z0-9-]+)"`));
      assert.ok(m, hits);
      assert.match(hits, new RegExp(`log ${today} › \\d{2}:\\d{2} · chatgpt-desktop · Fixed Atlas docker networking`));
      const one = await ok(claude, 'wiki_read', { target: m[1] });
      assert.match(one, new RegExp(`^File: log/\\d{4}/${today}\\.md#${m[1].split('#')[1]}\\n\\n# ${today}\\n\\n## \\d{2}:\\d{2} · chatgpt-desktop · Fixed Atlas docker networking\\n\\ntags:`));
      assert.equal(await ok(claude, 'wiki_read', { target: today, section: m[1].split('#')[1] }), one, 'the same as a section argument');
      const bad = await call(claude, 'wiki_read', { target: `${today}#no-such-entry` });
      assert.ok(bad.isError && /No section "no-such-entry".*Its sections: /.test(bad.text), bad.text);
      // Filters: dates around today keep it; a range before today, or another app, leave it out.
      assert.match(await ok(claude, 'wiki_search', { query: 'bridge network', since: today, until: today }), /Fixed Atlas docker networking/);
      assert.match(await ok(claude, 'wiki_search', { query: 'bridge network', until: '2000-01-01' }), /No results/);
      assert.match(await ok(claude, 'wiki_search', { query: 'bridge network', app: 'codex' }), /No results/);
      assert.match(await ok(claude, 'wiki_search', { query: 'bridge network', app: 'ChatGPT Desktop' }), /Fixed Atlas docker networking/, 'app names are normalized');
      const wrong = await call(claude, 'wiki_search', { query: 'x', since: 'yesterday' });
      assert.ok(wrong.isError && /pattern/.test(wrong.text), wrong.text);
      // Other wordings in `queries` are searched with the query and fused into one ranking.
      assert.match(await ok(claude, 'wiki_search', { query: 'zebra quokka marmalade' }), /No results .* try names, the broader topic/);
      const fused = await ok(claude, 'wiki_search', { query: 'zebra quokka marmalade', queries: ['atlas_net bridge', ''] });
      assert.match(fused, /result\(s\) for "zebra quokka marmalade" and 1 other wording\(s\):/);
      assert.match(fused, /Fixed Atlas docker networking/);
    });

    test('append, then replace, creates exactly one .history file with no colon', async () => {
      await ok(chatgpt, 'wiki_upsert_page', { app: 'chatgpt-desktop', title: 'Atlas', slug: 'atlas', content: 'Moved dev to Staging.' });
      let raw = fs.readFileSync(path.join(wikiDir, 'pages', 'atlas.md'), 'utf8');
      assert.match(raw, /\n### \d{4}-\d{2}-\d{2} \d{2}:\d{2} \(chatgpt-desktop\)\n\nMoved dev to Staging\.\n$/);
      assert.match(raw, /^updated_by: chatgpt-desktop$/m);
      const histDir = path.join(wikiDir, '.history', 'pages', 'atlas');
      assert.ok(!fs.existsSync(histDir) || fs.readdirSync(histDir).length === 0, 'append keeps no history');

      const r = await ok(claude, 'wiki_upsert_page', {
        app: 'claude-desktop',
        title: 'Atlas',
        slug: 'atlas',
        mode: 'replace',
        content: 'Rewritten: dev on Staging (previously production host).',
      });
      assert.match(r, /previous version kept at \.history\/pages\/atlas\//);
      const hist = fs.readdirSync(histDir);
      assert.equal(hist.length, 1, `history files: ${hist}`);
      assert.ok(!hist[0].includes(':'), hist[0]);
      assert.match(fs.readFileSync(path.join(histDir, hist[0]), 'utf8'), /Moved dev to Staging\./);
      raw = fs.readFileSync(path.join(wikiDir, 'pages', 'atlas.md'), 'utf8');
      assert.match(raw, /\n# Atlas\n\nRewritten: dev on Staging/);
      assert.ok(!raw.includes('Line one'));
      assert.match(raw, /^type: project$/m, 'type survives replace');
      assert.match(fs.readFileSync(todayLog, 'utf8'), /· claude-desktop · Page rewritten: Atlas \[\[atlas\]\]/);
    });

    test("wiki_start shows the index, related hits and both apps' activity", async () => {
      const text = await ok(chatgpt, 'wiki_start', { app: 'chatgpt-desktop', topic: 'docker networking' });
      assert.match(text, /## Page index \(1\)\n\n- atlas \[project\] Atlas - Audit app/);
      assert.match(text, /## Related to "docker networking"\n\n1\. /);
      assert.match(text, /chatgpt-desktop · Fixed Atlas docker networking/);
      assert.match(text, /claude-desktop · Page created: Atlas/);
      assert.match(text, /You are: chatgpt-desktop\./);
    });

    test('wiki_read works by slug and by date, and refuses paths outside the wiki', async () => {
      assert.match(await ok(claude, 'wiki_read', { target: 'atlas' }), /^File: pages\/atlas\.md\n\n---\ntitle: Atlas/);
      assert.match(await ok(claude, 'wiki_read', { target: today }), new RegExp(`^File: log/${today.slice(0, 4)}/${today}\\.md\\n\\n# ${today}`));
      assert.match(await ok(claude, 'wiki_read', { target: '[[atlas]]' }), /pages\/atlas\.md/);
      assert.match(await ok(claude, 'wiki_read', { target: 'index.md' }), /# Index/);
      assert.match(await ok(claude, 'wiki_read', { target: 'PROTOCOL' }), /# Agent Wiki protocol/);
      for (const bad of ['../../etc/passwd', '..\\..\\Windows\\win.ini', path.join(os.homedir(), '.ssh', 'config')]) {
        const r = await call(claude, 'wiki_read', { target: bad });
        assert.equal(r.isError, true, bad);
        assert.match(r.text, /outside the wiki/, bad);
      }
      const missing = await call(claude, 'wiki_read', { target: '1999-01-01' });
      assert.equal(missing.isError, true);
      assert.match(missing.text, /No log entries for 1999-01-01/);
    });

    test('secrets are refused and ordinary prose is allowed', async () => {
      const j = (...p) => p.join('');
      const secrets = {
        'private key': j('-----BEGIN ', 'RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----'),
        'openssh key': j('-----BEGIN ', 'OPENSSH PRIVATE KEY-----'),
        'sk- key': j('sk', '-proj-', 'Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4z'),
        'sk-ant- key': j('sk', '-ant-', 'api03-', 'Zx9Yw8Vu7Ts6Rq5Po4Nm3Lk2Ji1Hg0Fe'), // gitleaks:allow -- synthetic rejection fixture
        ghp: j('gh', 'p_', 'a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8'),
        gho: j('gh', 'o_', 'a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8'),
        ghs: j('gh', 's_', 'a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8'),
        github_pat: j('github', '_pat_', '11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz'),
        slack: j('xo', 'xb-', '123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx'),
        aws: j('AK', 'IA', 'IOSFODNN7EXAMPLE'),
        google: j('AI', 'za', 'SyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q'),
        jwt: j('eyJhbGciOiJIUzI1NiJ9', '.', 'eyJzdWIiOiIxMjM0NTY3ODkwIn0', '.', 'dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U'),
        npm: j('np', 'm_', 'a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8'),
        'visa card': j('card ', '4111 1111 ', '1111 1111'),
        'amex card': j('amex ', '3782822', '46310005'),
        ssn: j('SSN ', '123-45-', '6789'),
        'password assignment': j('password', ': ', 'Hunter2Hunter2xyz'),
        'api key assignment': j('api_key', '=', 'abc123def456ghi789'),
      };
      for (const [label, s] of Object.entries(secrets)) {
        const r = await call(chatgpt, 'wiki_log', { app: 'chatgpt-desktop', title: `Secret test ${label}`, body: `value ${s} end` });
        assert.equal(r.isError, true, `${label} should be refused`);
        assert.match(r.text, /Refused: .*Record WHERE the secret lives instead/, label);
      }
      assert.equal((await call(claude, 'wiki_upsert_page', { app: 'claude-desktop', title: 'Keys', content: `key ${secrets.ghp}` })).isError, true);
      assert.equal((await call(claude, 'wiki_log', { app: 'claude-desktop', title: `Rotated ${secrets.aws}` })).isError, true);

      const fine = [
        'the token refreshes every 8 hours',
        'Build 20261001134205123456 passed; artifact 4729381047561029384756 uploaded',
        "API key is in the 1Password vault 'Work'",
      ];
      for (const s of fine) await ok(chatgpt, 'wiki_log', { app: 'chatgpt-desktop', title: 'Fine prose', body: s });
      const log = fs.readFileSync(todayLog, 'utf8');
      assert.ok(!log.includes('Secret test'), 'no refused entry landed');
      for (const s of Object.values(secrets)) assert.ok(!log.includes(s));
      for (const s of fine) assert.ok(log.includes(s), s);
      assert.ok(!fs.existsSync(path.join(wikiDir, 'pages', 'keys.md')));
    });

    test('20 concurrent wiki_log calls split across both clients all land', async () => {
      const titles = Array.from({ length: 20 }, (_, i) => `Concurrent entry ${String(i).padStart(2, '0')}`);
      const results = await Promise.all(
        titles.map((title, i) => call(i % 2 ? chatgpt : claude, 'wiki_log', { app: i % 2 ? 'chatgpt-desktop' : 'claude-desktop', title })),
      );
      for (const r of results) assert.equal(r.isError, false, r.text);
      const log = fs.readFileSync(todayLog, 'utf8');
      for (const t of titles) assert.equal(log.split(`· ${t}\n`).length - 1, 1, `${t} appears exactly once`);
      assert.deepEqual(leftoverLocks(wikiDir), [], 'no leftover locks');
      assert.equal(fs.readdirSync(path.dirname(todayLog)).filter((f) => f.endsWith('.tmp')).length, 0, 'no temp files');
    });

    if (mode === 'http') {
      test('HTTP: /health reports version and wiki; bad Host, Origin, method and path are refused', async () => {
        const h = await rawRequest(httpServer.port);
        assert.equal(h.status, 200);
        const health = JSON.parse(h.body);
        assert.equal(health.ok, true);
        assert.equal(health.name, 'agent-wiki');
        assert.equal(health.wikiDir, wikiDir.replace(/\\/g, '/'));
        const init = JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'x', version: '1' } } });
        const mcpHeaders = { 'Content-Type': 'application/json', Accept: 'application/json, text/event-stream' };
        const rebinding = await rawRequest(httpServer.port, { method: 'POST', path: '/mcp', body: init, headers: { ...mcpHeaders, Host: `evil.example:${httpServer.port}` } });
        assert.equal(rebinding.status, 403, 'DNS-rebinding Host refused');
        const crossOrigin = await rawRequest(httpServer.port, { method: 'POST', path: '/mcp', body: init, headers: { ...mcpHeaders, Origin: 'https://evil.example' } });
        assert.equal(crossOrigin.status, 403, 'cross-origin request refused');
        const sameOrigin = await rawRequest(httpServer.port, { method: 'POST', path: '/mcp', body: init, headers: { ...mcpHeaders, Host: `localhost:${httpServer.port}`, Origin: `http://localhost:${httpServer.port}` } });
        assert.equal(sameOrigin.status, 200, sameOrigin.body);
        assert.equal((await rawRequest(httpServer.port, { method: 'GET', path: '/mcp' })).status, 405);
        assert.equal((await rawRequest(httpServer.port, { path: '/elsewhere' })).status, 404);
        assert.equal((await rawRequest(httpServer.port, { method: 'POST', path: '/mcp', body: '{not json', headers: mcpHeaders })).status, 400);
      });

      test('HTTP: closing stdin (the service stop signal) shuts the server down cleanly', async () => {
        const proc = httpServer.proc;
        const exited = new Promise((resolve) => proc.on('exit', (code) => resolve(code)));
        const t0 = Date.now();
        proc.stdin.end();
        const code = await Promise.race([exited, new Promise((r) => setTimeout(() => r('timeout'), 8000))]);
        assert.equal(code, 0, `exit code ${code}`);
        assert.ok(Date.now() - t0 < 6000);
        assert.deepEqual(leftoverLocks(wikiDir), [], 'no leftover locks');
      });
    }

    if (mode === 'stdio') describeHookTests({ env, wikiDir, getClient: () => claude });
  });
}

// ---------------------------------------------------------------- hook

function runHook(hookEnv, { payload } = {}) {
  return new Promise((resolve) => {
    const t0 = Date.now();
    const p = spawn(...hookCmd(runtime), { env: hookEnv, stdio: ['pipe', 'pipe', 'pipe'] });
    let out = '';
    p.stdout.on('data', (d) => (out += d));
    // stdin is deliberately left open, as some hosts do.
    if (payload) p.stdin.write(payload);
    const kill = setTimeout(() => p.kill(), 20_000);
    p.on('exit', (code) => {
      clearTimeout(kill);
      resolve({ code, out, ms: Date.now() - t0 });
    });
  });
}

const FALLBACK_RE = /installed but could not be read just now; call wiki_start if the tools are available/;

describe('the Stop hook nudge (M7, opt-in)', { skip: IMPL !== 'rust' && 'the Rust programs only' }, () => {
  test('once per session, when the transcript is long and has no wiki_log call', async () => {
    const home = path.join(tmp, 'nudge-home');
    fs.mkdirSync(home, { recursive: true });
    const stopEnv = { ...process.env, AGENT_WIKI_HOME: home, AGENT_WIKI_DIR: path.join(tmp, 'nudge-wiki') };
    const stop = (payload) =>
      new Promise((resolve) => {
        const p = spawn(rustBin(), ['hook', 'stop'], { env: stopEnv, stdio: ['pipe', 'pipe', 'pipe'] });
        let out = '';
        p.stdout.on('data', (d) => (out += d));
        p.stdin.write(JSON.stringify(payload)); // left open, as some hosts do
        const t0 = Date.now();
        p.on('exit', (code) => resolve({ code, out: out.trim(), ms: Date.now() - t0 }));
      });
    const line = (o) => `${JSON.stringify(o)}\n`;
    const long = path.join(tmp, 'long.jsonl');
    fs.writeFileSync(long, Array.from({ length: 40 }, (_, i) => line({ type: i % 2 ? 'assistant' : 'user', message: { content: `turn ${i}, which mentions wiki_log in passing` } })).join(''));
    const logged = path.join(tmp, 'logged.jsonl');
    fs.writeFileSync(logged, fs.readFileSync(long, 'utf8') + line({ type: 'assistant', message: { content: [{ type: 'tool_use', name: 'mcp__agent-wiki__wiki_log', input: {} }] } }));
    const short = path.join(tmp, 'short.jsonl');
    fs.writeFileSync(short, line({ type: 'user' }));
    const config = (nudge) => fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ hooks: { nudge } }));

    config(false);
    assert.equal((await stop({ session_id: 's1', transcript_path: long, stop_hook_active: false })).out, '', 'off unless hooks.nudge is true');
    config(true);
    assert.equal((await stop({ session_id: 's2', transcript_path: short })).out, '', 'a short session is left alone');
    const r = await stop({ session_id: 's2', transcript_path: long, stop_hook_active: false });
    assert.equal(r.code, 0);
    assert.ok(r.ms < 3000, `answers without waiting for stdin to close (${r.ms} ms)`);
    const out = JSON.parse(r.out);
    assert.equal(out.decision, 'block');
    assert.match(out.reason, /wiki_log/);
    assert.equal((await stop({ session_id: 's2', transcript_path: long })).out, '', 'never twice in a session');
    assert.equal((await stop({ session_id: 's3', transcript_path: long, stop_hook_active: true })).out, '', 'not while a Stop hook already continued the turn');
    assert.equal((await stop({ session_id: 's4', transcript_path: logged })).out, '', 'a session that logged is left alone');
    assert.equal((await stop({ session_id: 's5' })).out, '', 'no transcript, no nudge');
  });
});

function describeHookTests({ env, getClient }) {
  test('the hook emits valid JSON with pages and headlines', async () => {
    // Newest event: a compact page-change line, after the 20 full entries above.
    await ok(getClient(), 'wiki_upsert_page', { app: 'claude-code', title: 'Hook check', type: 'reference', content: 'x' });
    const payload = JSON.stringify({ session_id: 'test', hook_event_name: 'SessionStart', source: 'startup' });
    const r = await runHook(env, { payload });
    assert.equal(r.code, 0);
    const json = JSON.parse(r.out);
    assert.equal(json.hookSpecificOutput.hookEventName, 'SessionStart');
    const ctx = json.hookSpecificOutput.additionalContext;
    assert.match(ctx, /Call wiki_start once before your first substantive reply/);
    assert.match(ctx, /Pages \(2\): atlas, hook-check/);
    const headlines = ctx.split('\n').filter((l) => l.startsWith('- '));
    assert.equal(headlines.length, 15, 'capped at 15 headlines');
    assert.match(headlines[0], /^- \d{4}-\d{2}-\d{2} \d{2}:\d{2} · claude-code · Page created: Hook check \[\[hook-check\]\]$/);
    for (const h of headlines.slice(1)) assert.match(h, /· (claude|chatgpt)-desktop · Concurrent entry \d\d$/);
    assert.ok(r.ms < 8000, `took ${r.ms} ms`);
  });

  test('the hook falls back fast when the wiki path is under a FILE', async () => {
    const file = path.join(tmp, 'not-a-dir.txt');
    fs.writeFileSync(file, 'x');
    const r = await runHook({ ...env, AGENT_WIKI_DIR: path.join(file, 'wiki') });
    assert.equal(r.code, 0);
    assert.match(JSON.parse(r.out).hookSpecificOutput.additionalContext, FALLBACK_RE);
    assert.ok(r.ms < 8000, `took ${r.ms} ms`);
  });

  test('the hook falls back and exits in under 8 s when the path hangs', async () => {
    // A config.json that never answers: a Windows named pipe (or a POSIX FIFO)
    // that is opened but never written. The reader blocks indefinitely.
    let home;
    let cleanup;
    if (process.platform === 'win32') {
      home = `\\\\.\\pipe\\agent-wiki-e2e-${process.pid}`;
      const sockets = [];
      const srv = net.createServer((s) => sockets.push(s));
      await new Promise((res, rej) => srv.once('error', rej).listen(path.join(home, 'config.json'), res));
      cleanup = () => {
        sockets.forEach((s) => s.destroy());
        srv.close();
      };
    } else {
      home = path.join(tmp, 'fifo-home');
      fs.mkdirSync(home);
      execFileSync('mkfifo', [path.join(home, 'config.json')]);
      cleanup = () => {};
    }
    try {
      const hangEnv = { ...env, AGENT_WIKI_HOME: home };
      delete hangEnv.AGENT_WIKI_DIR;
      const r = await runHook(hangEnv, { payload: '{"hook_event_name":"SessionStart"}' });
      assert.equal(r.code, 0);
      assert.match(JSON.parse(r.out).hookSpecificOutput.additionalContext, FALLBACK_RE);
      assert.ok(r.ms >= 3500 && r.ms < 8000, `took ${r.ms} ms`); // it waited for the path, then gave up (about 4 s by its own clock)
    } finally {
      cleanup();
    }
  });
}
