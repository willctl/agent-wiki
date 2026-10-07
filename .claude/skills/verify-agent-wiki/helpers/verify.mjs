#!/usr/bin/env node
// Drives an isolated Agent Wiki for verification: its own copy of a wiki, its own home and port, the
// fake model unless --real-model. Every command appends what it sent and what came back to
// .tmp/verify-evidence/<run>/transcript.jsonl, which `stop` keeps. Run from the repository root:
//
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs start [--port 47897] [--run <name>] [--wiki <dir>] [--real-model]
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs doctor
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs mcp <tool> '<json arguments>'
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs curate [--lint]
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs api <GET|POST> <path> ['<json body>']
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs file <path inside the wiki>
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs tray
//   node .claude/skills/verify-agent-wiki/helpers/verify.mjs stop

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(fileURLToPath(new URL('../../../../', import.meta.url)));
const STATE = path.join(REPO, '.tmp', 'verify', 'state.json');
const EXE = process.platform === 'win32' ? '.exe' : '';
const [cmd, ...rest] = process.argv.slice(2);
const opt = (name, dflt) => (rest.includes(name) ? rest[rest.indexOf(name) + 1] : dflt);

function programs() {
  const env = process.env.AGENT_WIKI_RUST_BIN;
  const dirs = env ? [path.dirname(env)] : ['release', 'debug'].map((p) => path.join(REPO, 'rust', 'target', p));
  const dir = dirs.find((d) => fs.existsSync(path.join(d, `agent-wiki${EXE}`)));
  if (!dir) fail('No Rust build: run `cargo build --release` in rust/ (or set AGENT_WIKI_RUST_BIN).');
  return { agent: env || path.join(dir, `agent-wiki${EXE}`), tray: path.join(dir, `agent-wiki-tray${EXE}`) };
}

function fail(msg) {
  console.error(msg);
  process.exit(1);
}

const state = () => (fs.existsSync(STATE) ? JSON.parse(fs.readFileSync(STATE, 'utf8')) : null);
function need() {
  const s = state();
  if (!s) fail('No verification instance: run `verify.mjs start` first.');
  return s;
}

function record(s, entry) {
  fs.appendFileSync(path.join(s.evidence, 'transcript.jsonl'), `${JSON.stringify({ at: new Date().toISOString(), ...entry })}\n`);
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function status(port) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/status`, { signal: AbortSignal.timeout(3000) });
    return r.ok ? await r.json() : null;
  } catch {
    return null;
  }
}

async function start() {
  const old = state();
  if (old && alive(old.pid)) fail(`Instance "${old.run}" is already running on port ${old.port} (pid ${old.pid}). Run \`verify.mjs stop\` first.`);
  const port = Number(opt('--port', 47897));
  if (await status(port)) fail(`Port ${port} already answers /status: something else owns it. Pick another with --port.`);
  const run = opt('--run', new Date().toISOString().replace(/[:.]/g, '-'));
  const dir = path.join(REPO, '.tmp', 'verify', run);
  const evidence = path.join(REPO, '.tmp', 'verify-evidence', run);
  const wikiDir = path.join(dir, 'wiki');
  const home = path.join(dir, 'home');
  const ui = path.join(REPO, 'dist', 'runtime', 'ui');
  if (!fs.existsSync(path.join(ui, 'app.js'))) fail('The window is not built: run `npm run build`.');
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(home, { recursive: true });
  fs.mkdirSync(evidence, { recursive: true });
  fs.cpSync(path.resolve(opt('--wiki', path.join(REPO, 'eval', 'synthetic', 'wiki'))), wikiDir, { recursive: true, filter: (f) => !f.split(path.sep).includes('.locks') });
  const fake = { codexPath: path.join(REPO, 'test', 'fixtures', 'fake-codex.mjs'), debounceSeconds: 0.2, maxWaitSeconds: 1, pollSeconds: 1, maxAttempts: 2, timeoutSeconds: 60 };
  let curator = fake;
  if (rest.includes('--real-model')) {
    const { P, readJson } = await import(path.join(REPO, 'scripts', 'lib.mjs'));
    curator = ((await readJson(P.config).catch(() => null)) || {}).curator || {};
  }
  fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ writeMode: 'curated', port, curator }, null, 2));
  const env = { AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: home, AGENT_WIKI_UI_DIR: ui, FAKE_CODEX_LOG: path.join(evidence, 'model-calls.jsonl') };
  const log = fs.openSync(path.join(evidence, 'server.log'), 'a');
  const child = spawn(programs().agent, ['serve', '--http', '--port', String(port)], { env: { ...process.env, ...env }, detached: true, windowsHide: true, stdio: ['ignore', log, log] });
  child.unref();
  const s = { run, port, pid: child.pid, dir, wikiDir, home, evidence, env };
  for (let i = 0; i < 60 && !(await status(port)); i++) await new Promise((r) => setTimeout(r, 250));
  const st = await status(port);
  if (!st) fail(`The server did not answer on port ${port} within 15 s. See ${path.join(evidence, 'server.log')}.`);
  fs.mkdirSync(path.dirname(STATE), { recursive: true });
  fs.writeFileSync(STATE, JSON.stringify(s, null, 2));
  record(s, { cmd: 'start', port, pid: s.pid, version: st.version, wikiDir: st.wikiDir });
  console.log(`started "${run}": agent-wiki ${st.version} pid ${s.pid}\n  window   http://127.0.0.1:${port}/ui/\n  mcp      http://127.0.0.1:${port}/mcp\n  wiki     ${wikiDir}\n  evidence ${evidence}`);
}

async function doctor() {
  const s = need();
  const st = await status(s.port);
  const want = JSON.parse(fs.readFileSync(path.join(REPO, 'package.json'), 'utf8')).version;
  const problems = [];
  if (!st) problems.push(`nothing answers http://127.0.0.1:${s.port}/status`);
  else {
    if (st.pid !== s.pid) problems.push(`port ${s.port} is served by pid ${st.pid}, not ours (${s.pid})`);
    if (path.resolve(st.wikiDir || '') !== path.resolve(s.wikiDir)) problems.push(`it serves ${st.wikiDir}, not ${s.wikiDir}`);
    if (st.version !== want) problems.push(`version ${st.version}, but package.json says ${want}: rebuild the Rust programs`);
  }
  record(s, { cmd: 'doctor', problems, health: st?.health, reasons: st?.reasons });
  if (problems.length) fail(`doctor: not worth driving:\n- ${problems.join('\n- ')}`);
  console.log(`doctor: ok (agent-wiki ${st.version}, pid ${st.pid}, ${st.health}${st.reasons?.length ? `: ${st.reasons.join('; ')}` : ''})`);
}

async function mcp() {
  const s = need();
  const [tool, json = '{}'] = rest;
  if (!tool) fail('usage: verify.mjs mcp <tool> \'<json arguments>\'');
  const client = new Client({ name: 'verify-agent-wiki', version: '1' });
  await client.connect(new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${s.port}/mcp`)));
  try {
    const r = await client.callTool({ name: tool, arguments: JSON.parse(json) });
    const text = (r.content || []).map((c) => c.text ?? '').join('\n');
    record(s, { cmd: 'mcp', tool, arguments: JSON.parse(json), isError: !!r.isError, text: text.slice(0, 20_000) });
    console.log(text);
    if (r.isError) process.exitCode = 1;
  } finally {
    await client.close();
  }
}

function curate() {
  const s = need();
  const args = ['curator', '--once', ...(rest.includes('--lint') ? ['--lint'] : [])];
  const r = spawnSync(programs().agent, args, { env: { ...process.env, ...s.env }, encoding: 'utf8', timeout: 180_000 });
  record(s, { cmd: 'curate', args, code: r.status, stderr: (r.stderr || '').slice(-4000) });
  console.log(`curator ${args.slice(1).join(' ')}: exit ${r.status}`);
  if (r.status !== 0) fail(r.stderr || r.error?.message || 'curator failed');
}

async function api() {
  const s = need();
  const [method, p, body] = rest;
  if (!method || !p) fail("usage: verify.mjs api <GET|POST> <path> ['<json body>']");
  if (!p.startsWith('/')) fail(`The path must start with / (got ${p}). Git Bash rewrites /status into a Windows path: run with MSYS_NO_PATHCONV=1.`);
  const r = await fetch(`http://127.0.0.1:${s.port}${p}`, { method, headers: { 'X-Agent-Wiki': 'ui', 'Content-Type': 'application/json' }, body: body ?? (method === 'POST' ? '{}' : undefined) });
  const text = await r.text();
  record(s, { cmd: 'api', method, path: p, body: body ? JSON.parse(body) : undefined, status: r.status, response: text.slice(0, 20_000) });
  console.log(`${r.status} ${text}`);
  if (!r.ok) process.exitCode = 1;
}

function file() {
  const s = need();
  const rel = rest[0];
  const f = path.resolve(s.wikiDir, rel || '');
  if (!rel || !f.startsWith(path.resolve(s.wikiDir) + path.sep)) fail('usage: verify.mjs file <path inside the wiki>, e.g. pages/harbor.md');
  const text = fs.existsSync(f) ? fs.readFileSync(f, 'utf8') : null;
  record(s, { cmd: 'file', path: rel, exists: text !== null, text: text?.slice(0, 20_000) });
  console.log(text ?? `(no file ${rel})`);
}

function tray() {
  const s = need();
  const ini = path.join(s.dir, 'tray.ini');
  fs.writeFileSync(ini, [`wikiDir=${s.wikiDir}`, `logDir=${path.join(s.home, 'logs')}`, `icons=${path.join(REPO, 'dist', 'icons')}`, `port=${s.port}`, 'curator=0', 'icon=0'].join('\n'));
  const r = spawnSync(programs().tray, ['--config', ini, '--selftest'], { encoding: 'utf8', timeout: 30_000 });
  record(s, { cmd: 'tray', code: r.status, stdout: r.stdout, stderr: r.stderr });
  console.log(r.stdout || r.stderr);
}

async function stop() {
  const s = need();
  const st = await status(s.port);
  if (st && st.pid === s.pid) process.kill(s.pid);
  else if (alive(s.pid)) console.log(`pid ${s.pid} no longer serves port ${s.port}: left alone`);
  for (let i = 0; i < 40 && (await status(s.port)); i++) await new Promise((r) => setTimeout(r, 250));
  record(s, { cmd: 'stop' });
  fs.rmSync(s.dir, { recursive: true, force: true, maxRetries: 5 });
  fs.rmSync(STATE, { force: true });
  console.log(`stopped "${s.run}"; evidence kept in ${s.evidence}`);
}

const commands = { start, doctor, mcp, curate, api, file, tray, stop };
if (!commands[cmd]) fail(`usage: verify.mjs <${Object.keys(commands).join('|')}> ... (see the top of this file)`);
await commands[cmd]();
