// Unit tests for the installer's config-merge helpers.

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { parse } from 'smol-toml';
import { buildIcons } from '../scripts/build-icons.mjs';
import { IMPL, rustBin } from './impl.mjs';
import {
  BLOCK_END,
  BLOCK_START,
  compileService,
  hookCommand,
  P,
  removeOldCopies,
  replaceFile,
  rustTrayIni,
  removeBlock,
  renderPlugin,
  serviceIni,
  tomlRemoveApproval,
  tomlSetApproval,
  upsertBlock,
  waitForHealth,
} from '../scripts/lib.mjs';

const ID = 'agent-wiki@personal';

test('managed block: append, replace in place, remove', () => {
  const user = '# My rules\n\nBe terse.\n';
  const once = upsertBlock(user, 'v1 text');
  assert.equal(once, `# My rules\n\nBe terse.\n\n${BLOCK_START}\nv1 text\n${BLOCK_END}\n`);
  const withTail = `${once}\n## Later section\n`;
  const twice = upsertBlock(withTail, 'v2 text');
  assert.equal(twice, `# My rules\n\nBe terse.\n\n${BLOCK_START}\nv2 text\n${BLOCK_END}\n\n## Later section\n`);
  assert.equal(upsertBlock(twice, 'v2 text'), twice, 'idempotent');
  assert.equal(removeBlock(twice), '# My rules\n\nBe terse.\n\n## Later section\n');
  assert.equal(removeBlock(upsertBlock(null, 'only')), '');
  assert.equal(removeBlock(user), null);
});

const SAMPLE = `approval_policy = "on-request"
model = "gpt"

[desktop]
appearanceTheme = "dark"

[plugins."browser@openai-bundled"]
enabled = true

[mcp_servers.node_repl]
command = 'C:\\x\\node_repl.exe'

[mcp_servers.node_repl.env]
A = "1"
`;

test('config.toml approval: add, idempotent, update, remove round-trips', () => {
  const added = tomlSetApproval(SAMPLE, ID);
  const cfg = parse(added);
  assert.equal(cfg.plugins[ID].mcp_servers['agent-wiki'].default_tools_approval_mode, 'approve');
  assert.equal(cfg.mcp_servers.node_repl.env.A, '1', 'other tables intact');
  assert.ok(added.startsWith(SAMPLE.trimEnd()), 'appended, nothing rewritten');
  assert.equal(tomlSetApproval(added, ID), null, 'idempotent');
  const prompt = added.replace('default_tools_approval_mode = "approve"', 'default_tools_approval_mode = "prompt"');
  assert.equal(parse(tomlSetApproval(prompt, ID)).plugins[ID].mcp_servers['agent-wiki'].default_tools_approval_mode, 'approve');
  assert.equal(tomlRemoveApproval(added, ID), SAMPLE);
  assert.equal(tomlRemoveApproval(SAMPLE, ID), null);
});

test('config.toml approval: works when codex already wrote the plugin table', () => {
  const withPlugin = `${SAMPLE}\n[plugins."${ID}"]\nenabled = true\n`;
  const added = tomlSetApproval(withPlugin, ID);
  const cfg = parse(added);
  assert.equal(cfg.plugins[ID].enabled, true);
  assert.equal(cfg.plugins[ID].mcp_servers['agent-wiki'].default_tools_approval_mode, 'approve');
  const removed = tomlRemoveApproval(added, ID);
  assert.equal(parse(removed).plugins[ID].enabled, true);
  assert.equal(parse(removed).plugins[ID].mcp_servers, undefined);
});

test('config.toml approval refuses to write invalid TOML', () => {
  assert.throws(() => tomlSetApproval('a = [unclosed\n', ID), /not be valid TOML/);
});

test('plugin renders to exactly the source tree, twice in a row, with no placeholders', async (t) => {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-render-'));
  t.after(() => fsp.rm(root, { recursive: true, force: true }));
  const dest = path.join(root, 'plugins', 'agent-wiki');
  const map = {
    __NODE__: 'C:/Program Files/nodejs/node.exe',
    __RUNTIME__: 'C:/Users/x/.agent-wiki/runtime',
    __VERSION__: '9.9.9',
    __HOOK_COMMAND__: 'C:/PROGRA~1/nodejs/node.exe C:/Users/x/.agent-wiki/runtime/session-start.mjs',
    __POINTER__: 'POINTER TEXT',
    __PROTOCOL__: 'PROTOCOL "quoted" TEXT',
  };
  const first = await renderPlugin(dest, map);
  const second = await renderPlugin(dest, map); // re-render into an existing dir
  assert.deepEqual(second, first);
  assert.deepEqual(first, [
    '.claude-plugin/plugin.json',
    '.codex-plugin/plugin.json',
    '.mcp.json',
    'hooks/hooks.json',
    'skills/agent-wiki/SKILL.md',
  ]);
  const mcp = JSON.parse(await fsp.readFile(path.join(dest, '.mcp.json'), 'utf8'));
  assert.deepEqual(mcp.mcpServers['agent-wiki'], { command: map.__NODE__, args: [`${map.__RUNTIME__}/server.mjs`] });
  const hooks = JSON.parse(await fsp.readFile(path.join(dest, 'hooks', 'hooks.json'), 'utf8'));
  assert.equal(hooks.hooks.SessionStart[0].hooks[0].command, map.__HOOK_COMMAND__);
  const skill = await fsp.readFile(path.join(dest, 'skills', 'agent-wiki', 'SKILL.md'), 'utf8');
  assert.match(skill, /^---\nname: agent-wiki\ndescription: "/);
  assert.match(skill, /POINTER TEXT\n\nPROTOCOL "quoted" TEXT\n$/);
  assert.equal(JSON.parse(await fsp.readFile(path.join(dest, '.claude-plugin', 'plugin.json'), 'utf8')).version, '9.9.9');

  await renderPlugin(dest, map, { mcpServer: { type: 'http', url: 'http://127.0.0.1:47821/mcp' } });
  const httpMcp = JSON.parse(await fsp.readFile(path.join(dest, '.mcp.json'), 'utf8'));
  assert.deepEqual(httpMcp, { mcpServers: { 'agent-wiki': { type: 'http', url: 'http://127.0.0.1:47821/mcp' } } });
});

const freePort = () =>
  new Promise((resolve) => {
    const s = net.createServer().listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });

test('service wrapper: serves, restarts a killed server, reloads on upgrade, stops on stdin close', { skip: process.platform !== 'win32', timeout: 90_000 }, async () => {
  const repo = fileURLToPath(new URL('..', import.meta.url));
  const tmp = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-service-'));
  const runtime = path.join(tmp, 'runtime');
  await fsp.cp(path.join(repo, 'dist', 'runtime'), runtime, { recursive: true });
  const exe = path.join(tmp, 'AgentWikiService.exe');
  compileService(exe);
  const port = await freePort();
  const logDir = path.join(tmp, 'logs');
  const ini = path.join(tmp, 'AgentWikiService.ini');
  const configDir = path.join(tmp, 'config');
  // The server must find config.json in the ini's configDir: writeMode "direct" in /health shows it read that one.
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(path.join(configDir, 'config.json'), JSON.stringify({ writeMode: 'direct' }));
  fs.writeFileSync(ini, serviceIni({ node: process.execPath, runtime, wikiDir: path.join(tmp, 'wiki'), configDir, dataDir: tmp, stateDir: path.join(tmp, 'state'), port, logDir }));

  const wrapper = spawn(exe, ['--console', '--config', ini], { stdio: ['pipe', 'pipe', 'pipe'] });
  const exited = new Promise((resolve) => wrapper.on('exit', (code) => resolve(code)));
  try {
    const first = await waitForHealth(port);
    assert.equal(first?.ok, true, 'healthy after start');
    assert.equal(first.writeMode, 'direct', 'config.json read from the ini configDir');

    process.kill(first.pid); // simulate a crash
    const second = await waitForHealth(port, (h) => h?.ok && h.pid !== first.pid);
    assert.ok(second?.ok && second.pid !== first.pid, 'restarted after crash');

    const now = Date.now() / 1000;
    fs.utimesSync(path.join(runtime, 'server.mjs'), now, now); // simulate an upgrade
    const third = await waitForHealth(port, (h) => h?.ok && h.pid !== second.pid);
    assert.ok(third?.ok && third.pid !== second.pid, 'restarted after server.mjs changed');

    wrapper.stdin.end(); // console-mode stop signal
    const code = await Promise.race([exited, new Promise((r) => setTimeout(() => r('timeout'), 15_000))]);
    assert.equal(code, 0, `wrapper exit ${code}`);
    assert.equal(await waitForHealth(port, () => true, 1000), null, 'server is gone');
    const log = fs.readFileSync(path.join(logDir, 'service.log'), 'utf8');
    assert.match(log, /\[service\] started server pid \d+/);
    assert.match(log, /server exited with code \d+ after \d+s; restarting/);
    assert.match(log, /server\.mjs changed on disk; restarting the server/);
    assert.match(log, /listening on http:\/\/127\.0\.0\.1:\d+\/mcp/);
    assert.match(log, /shutting down \(parent closed stdin\)/);
    assert.match(log, /\[service\] stopped/);
    assert.ok(fs.readdirSync(logDir).some((n) => /^requests-\d{4}-\d{2}-\d{2}\.jsonl$/.test(n)), 'the request log is in the ini logDir');
  } finally {
    if (wrapper.exitCode === null) wrapper.kill();
    await fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {});
  }
});

test('service ACE for the tray: added once, kept before the SACL, removable', async () => {
  const { sddlWithStartStop, sddlWithoutStartStop, trayIni } = await import('../scripts/lib.mjs');
  const sid = 'S-1-12-1-1-2-3-4';
  const sd = 'D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)';
  const added = sddlWithStartStop(sd, sid);
  assert.equal(added, `${sd}(A;;RPWPLC;;;${sid})`);
  assert.equal(sddlWithStartStop(added, sid), added, 'idempotent');
  const asStored = `${sd}(A;;LCRPWP;;;${sid})S:(AU;FA;CCDCLCSWRPWPDTLOSDRCWDWO;;;WD)`; // what sc sdshow printed on 2026-10-02
  assert.equal(sddlWithStartStop(asStored, sid), asStored, 'the same rights in Windows\' order count as granted');
  assert.equal(sddlWithStartStop(sd.replace('(A;;CCLCSWLOCRRC;;;SU)', `(A;;CCDC;;;${sid})(A;;CCLCSWLOCRRC;;;SU)`), sid), added, 'replaces an older ACE for the same SID');
  const withSacl = `${sd}S:(AU;FA;CCDCLCSWRPWPDTLOSDRCWDWO;;;WD)`;
  assert.equal(sddlWithStartStop(withSacl, sid), `${sd}(A;;RPWPLC;;;${sid})S:(AU;FA;CCDCLCSWRPWPDTLOSDRCWDWO;;;WD)`);
  assert.equal(sddlWithoutStartStop(added, sid), sd);
  assert.throws(() => sddlWithStartStop(sd, 'not-a-sid'), /not a SID/);
  const ini = trayIni({ node: 'C:/n/node.exe', runtime: 'C:/r', wikiDir: 'C:/w', uiProfile: 'C:/h/ui-profile', logDir: 'C:/h/logs', icons: 'C:/h/tray/icons', port: 47821, codex: 'C:/c/codex.exe', codexHome: 'C:/h/curator/codex-home', taskXml: 'C:/h/tray/AgentWikiTray.task.xml' });
  assert.ok(ini.split('\n').includes('node=C:\\n\\node.exe'), ini);
  assert.match(ini, /^port=47821$/m);
  assert.ok(ini.split('\n').includes('uiProfile=C:\\h\\ui-profile'), ini);
  assert.doesNotMatch(ini, /^home=/m);
  assert.ok(ini.split('\n').includes('codexHome=C:\\h\\curator\\codex-home'), ini);
  assert.match(ini, /^service=AgentWiki$/m);
  assert.match(ini, /^task=AgentWikiTray$/m);
  assert.match(ini, /^runValue=AgentWikiTray$/m);
  assert.ok(ini.split('\n').includes('taskXml=C:\\h\\tray\\AgentWikiTray.task.xml'), ini);
});

test('Claude Code allow rules: added once next to your own, removed cleanly', async () => {
  const { CLAUDE_ALLOW, claudeAllowRemove, claudeAllowSet } = await import('../scripts/lib.mjs');
  assert.deepEqual(CLAUDE_ALLOW, ['mcp__plugin_agent-wiki_agent-wiki', 'mcp__agent-wiki']);
  const mine = { enabledPlugins: { 'agent-wiki@agent-wiki-local': true }, permissions: { allow: ['Bash(git status)'], deny: ['Read(./.env)'] }, theme: 'dark' };
  const added = claudeAllowSet(JSON.stringify(mine, null, 2));
  const s = JSON.parse(added);
  assert.deepEqual(s.permissions.allow, ['Bash(git status)', ...CLAUDE_ALLOW], 'appended after your rules');
  assert.deepEqual(s.permissions.deny, ['Read(./.env)']);
  assert.equal(s.theme, 'dark');
  assert.equal(claudeAllowSet(added), null, 'idempotent');
  assert.deepEqual(JSON.parse(claudeAllowRemove(added)), mine, 'removal restores your settings');
  assert.equal(claudeAllowRemove(JSON.stringify(mine)), null, 'nothing to remove');
  const fresh = claudeAllowSet(null);
  assert.deepEqual(JSON.parse(fresh), { permissions: { allow: CLAUDE_ALLOW } }, 'works without a settings file');
  assert.deepEqual(JSON.parse(claudeAllowRemove(fresh)), {}, 'drops an emptied permissions block');
  const partial = JSON.stringify({ permissions: { allow: ['mcp__agent-wiki'] } });
  assert.deepEqual(JSON.parse(claudeAllowSet(partial)).permissions.allow, ['mcp__agent-wiki', 'mcp__plugin_agent-wiki_agent-wiki'], 'adds only what is missing');
});

test('logon task for the tray: as you, at your sign-in, normal priority, no time limit, one instance', async () => {
  const { trayTaskXml, taskXmlBytes } = await import('../scripts/lib.mjs');
  const sid = 'S-1-12-1-953236009-1184491680-746843566-3387050240';
  const xml = trayTaskXml({ exe: 'C:/Users/A & B/.agent-wiki/tray/AgentWikiTray.exe', sid });
  const one = (tag) => {
    const all = [...xml.matchAll(new RegExp(`<${tag}>([^<]*)</${tag}>`, 'g'))].map((m) => m[1]);
    assert.equal(all.length, 1, `${tag} appears once`);
    return all[0];
  };
  assert.match(xml, /^<\?xml version="1\.0" encoding="UTF-16"\?>\n<Task version="1\.2" /);
  assert.match(xml, new RegExp(`<LogonTrigger>\\s*<Enabled>true</Enabled>\\s*<UserId>${sid}</UserId>`), 'triggers on your sign-in only');
  assert.match(xml, new RegExp(`<Principal id="Author">\\s*<UserId>${sid}</UserId>\\s*<LogonType>InteractiveToken</LogonType>\\s*<RunLevel>LeastPrivilege</RunLevel>`));
  assert.equal(one('ExecutionTimeLimit'), 'PT0S', 'the default would stop the tray after 72 h');
  assert.equal(one('Priority'), '4', 'the default 7 is below normal');
  assert.equal(one('MultipleInstancesPolicy'), 'IgnoreNew');
  assert.equal(one('DisallowStartIfOnBatteries'), 'false');
  assert.equal(one('StopIfGoingOnBatteries'), 'false');
  assert.equal(one('Command'), '"C:\\Users\\A &amp; B\\.agent-wiki\\tray\\AgentWikiTray.exe"');
  assert.equal(one('Arguments'), '--from task');
  assert.equal(one('WorkingDirectory'), 'C:\\Users\\A &amp; B\\.agent-wiki\\tray');
  assert.ok(!xml.includes('\r'), 'LF');
  assert.match(trayTaskXml({ exe: 'C:/x.exe', sid, enabled: false }), /<Settings>[\s\S]*<Enabled>false<\/Enabled>[\s\S]*<\/Settings>/);
  assert.throws(() => trayTaskXml({ exe: 'C:/x.exe', sid: 'nobody' }), /not a SID/);
  const bytes = taskXmlBytes(xml);
  assert.deepEqual([...bytes.subarray(0, 2)], [0xff, 0xfe], 'UTF-16LE BOM: schtasks rejects UTF-8');
  assert.equal(bytes.subarray(2).toString('utf16le'), xml);
});

test('Agent Wiki window profile: Edge is found, and a headless first start prepares the profile once', { skip: process.platform !== 'win32', timeout: 90_000 }, async () => {
  const { edgePath, regValue, warmUiProfile } = await import('../scripts/lib.mjs');
  assert.equal(regValue('HKCU\\Software\\AgentWikiNoSuchKey', null), null);
  const edge = edgePath();
  if (!edge) return; // no Edge on this machine: the tray falls back to the default browser
  assert.match(edge, /msedge\.exe$/i);
  const dir = path.join(fs.realpathSync.native(os.tmpdir()), `aw-ui-profile-${process.pid}`);
  try {
    assert.equal(warmUiProfile(dir), 'created');
    assert.ok(fs.existsSync(path.join(dir, 'Local State')));
    assert.equal(warmUiProfile(dir), 'ready', 'only once');
  } finally {
    await fsp.rm(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 }).catch(() => {});
  }
});

test('logon task: Task Scheduler takes it without admin; query and delete', { skip: process.platform !== 'win32' }, async () => {
  const { currentUserSid, deleteTask, queryTask, registerTask, taskXmlBytes, trayTaskXml } = await import('../scripts/lib.mjs');
  const name = `AgentWikiTest-${process.pid}`;
  const dir = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-task-'));
  const xmlFile = path.join(dir, 'task.xml');
  const exe = path.join(dir, 'AgentWikiTray.exe');
  try {
    // Disabled, so it can never run at a real sign-in even if cleanup fails.
    fs.writeFileSync(xmlFile, taskXmlBytes(trayTaskXml({ exe, sid: currentUserSid(), enabled: false })));
    assert.deepEqual(queryTask(name), { exists: false, enabled: false, command: null });
    registerTask(name, xmlFile);
    registerTask(name, xmlFile); // replaces in place
    const q = queryTask(name);
    assert.equal(q.exists, true);
    assert.equal(q.enabled, false);
    assert.equal(path.resolve(q.command), path.resolve(exe));
  } finally {
    assert.equal(deleteTask(name), true);
    await fsp.rm(dir, { recursive: true, force: true });
  }
  assert.equal(queryTask(name).exists, false);
  assert.equal(deleteTask(name), false, 'deleting a missing task reports false');
});

test('replaceFile: installs, leaves an identical file alone, moves a running program aside', { timeout: 60_000 }, async () => {
  const dir = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-replace-'));
  try {
    const exe = process.platform === 'win32' ? '.exe' : '';
    const a = path.join(dir, `a${exe}`);
    const b = path.join(dir, 'b.txt');
    const dest = path.join(dir, 'bin', `prog${exe}`);
    await fsp.copyFile(process.execPath, a);
    await fsp.writeFile(b, 'another build');
    assert.equal(await replaceFile(a, dest), 'installed');
    assert.equal(await replaceFile(a, dest), 'unchanged');
    // The installed program runs (a service, a curator): Windows refuses to overwrite it, renaming works.
    const child = spawn(dest, ['-e', 'setTimeout(() => {}, 20000)'], { stdio: 'ignore' });
    await new Promise((r) => setTimeout(r, 500));
    try {
      const how = await replaceFile(b, dest);
      assert.match(how, process.platform === 'win32' ? /^replaced/ : /^(installed|replaced)/);
      assert.equal(await fsp.readFile(dest, 'utf8'), 'another build');
      if (process.platform === 'win32') assert.equal(fs.readdirSync(path.dirname(dest)).filter((f) => f.includes('.old-')).length, 1, 'the running copy waits aside');
    } finally {
      child.kill();
      await new Promise((r) => child.on('exit', r));
    }
    await new Promise((r) => setTimeout(r, 300));
    await removeOldCopies(dest);
    assert.deepEqual(fs.readdirSync(path.dirname(dest)), [path.basename(dest)], 'removed once nothing runs it');
  } finally {
    await fsp.rm(dir, { recursive: true, force: true, maxRetries: 5 });
  }
});

test('Rust programs: the hook runs agent-wiki, the tray settings name it', () => {
  assert.match(hookCommand(true), /agent-wiki(\.exe)?["']? hook$/); // quoted on Windows ("") and POSIX ('') when the path has spaces
  assert.match(hookCommand(false), /session-start\.mjs["']?$/);
  const ini = rustTrayIni({ agent: P.agentExe, wikiDir: '/w', logDir: '/l', icons: '/i', port: 47821, codex: 'codex', codexHome: '/h', taskXml: '/t.xml', webviewDir: '/v' });
  for (const k of ['agent=', 'wikiDir=', 'logDir=', 'icons=', 'webviewDir=', 'port=47821', 'codex=codex']) assert.ok(ini.includes(k), k);
  assert.doesNotMatch(ini, /^node=|^runtime=|^uiProfile=/m, 'none of the Node or Edge settings');
});

test('the Rust installer: a sandboxed install merges into your files, re-runs as a no-op, and uninstalls cleanly', { skip: IMPL !== 'rust', timeout: 180_000 }, async () => {
  const repo = fileURLToPath(new URL('..', import.meta.url));
  if (!fs.existsSync(path.join(repo, 'dist', 'icons', 'agent-wiki-healthy.ico'))) buildIcons({ outDir: path.join(repo, 'dist', 'icons'), preview: false });
  const sb = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-rust-install-'));
  // The payload a release ships: both programs, the window's files and the icons, in one folder.
  const from = path.join(sb, 'payload');
  const exe = process.platform === 'win32' ? '.exe' : '';
  await fsp.mkdir(from, { recursive: true });
  for (const name of [`agent-wiki${exe}`, `agent-wiki-tray${exe}`, 'WebView2Loader.dll']) {
    const src = path.join(path.dirname(rustBin()), name);
    if (fs.existsSync(src)) await fsp.copyFile(src, path.join(from, name));
  }
  await fsp.cp(path.join(repo, 'dist', 'runtime', 'ui'), path.join(from, 'ui'), { recursive: true });
  await fsp.cp(path.join(repo, 'dist', 'icons'), path.join(from, 'icons'), { recursive: true });
  const bin = path.join(from, `agent-wiki${exe}`);
  const run = (cmd) => spawnSync(bin, [cmd, '--sandbox', sb, '--from', from], { encoding: 'utf8', timeout: 120_000 });
  try {
    // Your own files: instructions with your text, and a Claude desktop config (CRLF) with another server.
    const claudeMd = path.join(sb, '.claude', 'CLAUDE.md');
    await fsp.mkdir(path.dirname(claudeMd), { recursive: true });
    await fsp.writeFile(claudeMd, '# Mine\n\nBe terse.\n');
    const desktopDir =
      process.platform === 'win32' ? path.join(sb, 'AppData', 'Roaming', 'Claude') : process.platform === 'darwin' ? path.join(sb, 'Library', 'Application Support', 'Claude') : path.join(sb, '.config', 'Claude');
    const desktop = path.join(desktopDir, 'claude_desktop_config.json');
    await fsp.mkdir(desktopDir, { recursive: true });
    const mine = '{\r\n  "mcpServers": {\r\n    "other": {\r\n      "command": "x"\r\n    }\r\n  }\r\n}\r\n';
    await fsp.writeFile(desktop, mine);

    let r = run('install');
    assert.equal(r.status, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /MCP over stdio OK: agent-wiki \d+\.\d+\.\d+, 5 tools, wiki_start OK/);
    assert.match(r.stdout, /hook OK under/);
    const text = fs.readFileSync(desktop, 'utf8');
    assert.ok(text.includes('\r\n'), 'your line endings are kept');
    const d = JSON.parse(text);
    assert.deepEqual(Object.keys(d.mcpServers), ['other', 'agent-wiki']);
    assert.deepEqual(d.mcpServers['agent-wiki'].args, ['serve']);
    assert.ok(fs.existsSync(`${desktop}.bak-agent-wiki`), 'backed up once');
    assert.match(fs.readFileSync(claudeMd, 'utf8'), /^# Mine\n\nBe terse\.\n\n<!-- AGENT-WIKI:START/);
    const findRendered = (dir) => {
      for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        const p = path.join(dir, e.name);
        if (e.isDirectory() && e.name === 'agent-wiki' && fs.existsSync(path.join(p, '.mcp.json'))) return p;
        if (e.isDirectory()) {
          const f = findRendered(p);
          if (f) return f;
        }
      }
      return null;
    };
    const rendered = findRendered(sb);
    const mcp = JSON.parse(fs.readFileSync(path.join(rendered, '.mcp.json'), 'utf8'));
    assert.match(mcp.mcpServers['agent-wiki'].command, /agent-wiki(\.exe)?$/);
    assert.match(JSON.parse(fs.readFileSync(path.join(rendered, 'hooks', 'hooks.json'), 'utf8')).hooks.SessionStart[0].hooks[0].command, / hook$/);
    assert.ok(fs.existsSync(path.join(sb, 'AgentWiki', 'index.md')), 'the wiki is set up');

    r = run('install');
    assert.equal(r.status, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /agent-wiki(\.exe)?: unchanged/);
    assert.match(r.stdout, /managed block already current/);

    // While Claude desktop runs (in the sandbox: this file), its config is left alone. Nothing for you to
    // do when it already has this install's entry; a step for you when it does not.
    const running = path.join(sb, 'claude-desktop-running');
    await fsp.writeFile(running, '');
    const current = fs.readFileSync(desktop, 'utf8');
    r = run('install');
    assert.equal(r.status, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /ok\s+Claude desktop MCP: already registered/);
    assert.equal(fs.readFileSync(desktop, 'utf8'), current);
    const stale = JSON.parse(current);
    stale.mcpServers['agent-wiki'].command = 'C:/old/agent-wiki.exe';
    await fsp.writeFile(desktop, JSON.stringify(stale));
    r = run('install');
    assert.equal(r.status, 0, r.stdout + r.stderr);
    assert.match(r.stdout, /manual\s+Claude desktop MCP: quit Claude desktop/);
    assert.equal(JSON.parse(fs.readFileSync(desktop, 'utf8')).mcpServers['agent-wiki'].command, 'C:/old/agent-wiki.exe', 'not edited while it runs');
    await fsp.writeFile(desktop, current);
    await fsp.rm(running);

    r = run('uninstall');
    assert.equal(r.status, 0, r.stdout + r.stderr);
    assert.equal(fs.readFileSync(claudeMd, 'utf8'), '# Mine\n\nBe terse.\n', 'your text, without the block');
    assert.equal(fs.readFileSync(desktop, 'utf8'), mine, 'your config as it was');
    assert.ok(fs.existsSync(path.join(sb, 'AgentWiki', 'index.md')), 'the wiki is never deleted');

    // Another app's config that is valid JSON but not an object stops the install and names the file.
    await fsp.writeFile(desktop, '[]');
    r = run('install');
    assert.notEqual(r.status, 0);
    assert.match(r.stdout + r.stderr, /claude_desktop_config\.json is not a JSON object/);
    assert.doesNotMatch(r.stderr, /panicked/);
    assert.equal(fs.readFileSync(desktop, 'utf8'), '[]', 'left as it was');
  } finally {
    await fsp.rm(sb, { recursive: true, force: true, maxRetries: 5 });
  }
});
