// Ask (agentic search), end to end against the BUNDLED runtime and the fake model
// (test/fixtures/fake-codex.mjs, which drives the real read-only MCP server the way Codex does):
// the read-only server, the worker inside the curator process, the window's /api/ask endpoints,
// live steps, sources, follow-ups, stopping, failures, expiry and the request log.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { curatorCmd, srv, srvT } from './impl.mjs';

const repo = fileURLToPath(new URL('..', import.meta.url));
const dist = path.join(repo, 'dist', 'runtime');
if (!fs.existsSync(path.join(dist, 'curator.mjs'))) throw new Error('Run `npm run build` first.');
const fakeCodex = path.join(repo, 'test', 'fixtures', 'fake-codex.mjs');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const SECRET = ['AKIA', 'IOSFODNN7EXAMPLE'].join('');

let tmp;
let runtime;
before(async () => {
  tmp = fs.realpathSync.native(await fsp.mkdtemp(path.join(os.tmpdir(), 'agent-wiki-ask-')));
  runtime = path.join(tmp, 'runtime');
  await fsp.cp(dist, runtime, { recursive: true });
});
if (!process.env.AGENT_WIKI_KEEP_TEST_TMP) after(() => fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {}));

/** A wiki with two pages and a log day, a home whose curator uses the fake model. */
function makeWiki(name) {
  const dir = path.join(tmp, name);
  const wikiDir = path.join(dir, 'wiki');
  const home = path.join(dir, 'home');
  fs.mkdirSync(path.join(wikiDir, 'pages'), { recursive: true });
  fs.mkdirSync(home, { recursive: true });
  const page = (slug, title, type, body) =>
    fs.writeFileSync(path.join(wikiDir, 'pages', `${slug}.md`), `---\ntitle: ${title}\ntype: ${type}\nsummary: About ${title}\ntags: []\n---\n\n# ${title}\n\n${body}\n`);
  page('alpha', 'Alpha', 'project', 'The tray logon task is called AgentWikiTray in Task Scheduler.\n\nLinks to [[beta]].');
  page('beta', 'Beta', 'reference', 'Request logs live in ~/.agent-wiki/logs.');
  fs.writeFileSync(
    path.join(home, 'config.json'),
    JSON.stringify({ writeMode: 'curated', curator: { codexPath: fakeCodex, lint: 'off', pollSeconds: 1, timeoutSeconds: 20 }, ask: { timeoutSeconds: 20, queueSeconds: 600 } }),
  );
  const modeFile = path.join(dir, 'fake-mode.txt');
  const env = {
    ...process.env,
    AGENT_WIKI_DIR: wikiDir,
    AGENT_WIKI_HOME: home,
    FAKE_CODEX_LOG: path.join(dir, 'fake-codex.jsonl'),
    FAKE_CODEX_PROMPTS: path.join(dir, 'prompts'),
    FAKE_CODEX_MODE_FILE: modeFile,
  };
  delete env.AGENT_WIKI_WRITE_MODE;
  delete env.AGENT_WIKI_FAULT;
  delete env.FAKE_CODEX_MODE;
  return { dir, wikiDir, home, env, modeFile, logs: path.join(home, 'logs'), setMode: (m) => fs.writeFileSync(modeFile, m) };
}

function startHttp(env) {
  return new Promise((resolve, reject) => {
    const proc = spawn(...srv(runtime, '--http', '--port', '0', '--parent-stdin'), { env, stdio: ['pipe', 'pipe', 'pipe'] });
    let err = '';
    const timer = setTimeout(() => reject(new Error(`server did not start:\n${err}`)), 10_000);
    proc.stderr.setEncoding('utf8');
    proc.stderr.on('data', (d) => {
      err += d;
      const m = err.match(/listening on http:\/\/127\.0\.0\.1:(\d+)\/mcp/);
      if (m) {
        clearTimeout(timer);
        resolve({ proc, port: Number(m[1]) });
      }
    });
  });
}

function startCurator(env) {
  const proc = spawn(...curatorCmd(runtime, '--parent-stdin'), { env, stdio: ['pipe', 'pipe', 'pipe'] });
  proc.stderr.setEncoding('utf8');
  let err = '';
  proc.stderr.on('data', (d) => (err += d));
  const exited = new Promise((r) => proc.on('exit', r));
  return { proc, stderr: () => err, stop: () => (proc.stdin.end(), exited) };
}

function client(port) {
  const req = (method, p, { headers = {}, body } = {}) =>
    new Promise((resolve, reject) => {
      const r = http.request({ host: '127.0.0.1', port, path: p, method, headers: { Host: `127.0.0.1:${port}`, ...headers } }, (res) => {
        let text = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (text += c));
        res.on('end', () => resolve({ status: res.statusCode, json: /json/.test(res.headers['content-type'] || '') ? JSON.parse(text) : null }));
      });
      r.on('error', reject);
      if (body !== undefined) r.write(typeof body === 'string' ? body : JSON.stringify(body));
      r.end();
    });
  const UI = { 'X-Agent-Wiki': 'ui', 'Content-Type': 'application/json' };
  return {
    req,
    ask: (question, parent) => req('POST', '/api/ask', { headers: UI, body: { question, ...(parent ? { parent } : {}) } }),
    cancel: (id) => req('POST', '/api/ask/cancel', { headers: UI, body: { id } }),
    get: (id, q = '') => req('GET', `/api/ask?id=${id}${q}`),
    /** Polls like the window does (after=<count>) until the question settles; returns it with every event. */
    async settle(id, { timeoutMs = 30_000, until = (a) => !['queued', 'running'].includes(a.status) } = {}) {
      let after = 0;
      const events = [];
      const deadline = Date.now() + timeoutMs;
      for (;;) {
        const a = (await req('GET', `/api/ask?id=${id}&after=${after}`)).json;
        events.push(...a.events);
        after = a.eventCount;
        if (until({ ...a, events })) return { ...a, events };
        if (Date.now() > deadline) throw new Error(`question ${id} still ${a.status} after ${timeoutMs} ms`);
        await sleep(100);
      }
    },
    async workerUp(timeoutMs = 15_000) {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if ((await req('GET', '/api/status')).json.ask?.running) return;
        await sleep(100);
      }
      throw new Error('the Ask worker did not start');
    },
  };
}

const requestLog = (logs) =>
  fs
    .readdirSync(logs)
    .filter((n) => n.startsWith('requests-'))
    .flatMap((n) => fs.readFileSync(path.join(logs, n), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)));

/** Every file under `dir` with its contents, except Ask's own files and the curator's heartbeat and locks. */
function snapshot(dir) {
  const out = {};
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      const rel = path.relative(dir, p).replace(/\\/g, '/');
      if (/^(\.curator\/(asks|tmp|status\.json)|\.locks)(\/|$)/.test(rel)) continue;
      if (e.isDirectory()) walk(p);
      else out[rel] = fs.readFileSync(p, 'utf8');
    }
  };
  walk(dir);
  return out;
}

describe('the read-only server (the agent’s only tools)', { timeout: 30_000 }, () => {
  test('offers wiki_search and wiki_read only, answers them, and writes nothing to the wiki', async () => {
    const W = makeWiki('readonly');
    const before = snapshot(W.wikiDir);
    const c = new Client({ name: 'codex-mcp-client', version: 'test' });
    await c.connect(new StdioClientTransport({ ...srvT(runtime, '--read-only'), env: { ...W.env, AGENT_WIKI_PROCESS: 'ask', AGENT_WIKI_ASK: 'test-ask' }, stderr: 'pipe' }));
    try {
      assert.deepEqual((await c.listTools()).tools.map((t) => t.name).sort(), ['wiki_read', 'wiki_search']);
      const s = await c.callTool({ name: 'wiki_search', arguments: { query: 'logon task' } });
      assert.match(s.content[0].text, /pages\/alpha\.md - Alpha \[project\] \(read: "alpha"/);
      const r = await c.callTool({ name: 'wiki_read', arguments: { target: 'alpha' } });
      assert.match(r.content[0].text, /^File: pages\/alpha\.md/);
      const w = await c.callTool({ name: 'wiki_log', arguments: { app: 'x', title: 'nope' } }).catch((e) => ({ isError: true, content: [{ text: e.message }] }));
      assert.equal(w.isError, true, 'wiki_log is not there to call');
      assert.match(w.content[0].text, /wiki_log|not found/i);
    } finally {
      await c.close();
    }
    assert.deepEqual(snapshot(W.wikiDir), before, 'no skeleton, lock, index or note was written');
    const lines = requestLog(W.logs).filter((l) => l.kind === 'tool');
    assert.deepEqual(lines.map((l) => [l.proc, l.tool, l.ask]), [['ask', 'wiki_search', 'test-ask'], ['ask', 'wiki_read', 'test-ask']]);
  });
});

describe('Ask end to end: the window, the service and the curator', { timeout: 120_000 }, () => {
  let W;
  let server;
  let api;
  let curator;
  before(async () => {
    W = makeWiki('e2e');
    server = await startHttp(W.env);
    api = client(server.port);
  });
  after(async () => {
    await curator?.stop();
    server?.proc.kill();
  });

  test('without the curator nobody can answer: refused with the reason; the header and JSON are required', async () => {
    const r = await api.ask('Where is the logon task?');
    assert.equal(r.status, 503);
    assert.match(r.json.error, /curator.*tray/i);
    assert.equal((await api.req('POST', '/api/ask', { body: { question: 'x' } })).status, 403, 'no X-Agent-Wiki header');
    assert.equal((await api.req('POST', '/api/ask', { headers: { 'X-Agent-Wiki': 'ui', Origin: 'https://evil.example' }, body: { question: 'x' } })).status, 403);
    assert.equal((await api.req('POST', '/api/ask', { headers: { 'X-Agent-Wiki': 'ui' }, body: 'not json' })).status, 400);
    assert.deepEqual((await api.req('GET', '/api/asks')).json, { asks: [], worker: { running: false } });
  });

  test('a question: live steps (search, read), then the answer with its sources, filed nowhere', async () => {
    curator = startCurator(W.env);
    await api.workerUp();
    W.setMode('normal');
    const before = snapshot(W.wikiDir);
    const r = await api.ask('What is the tray logon task called?');
    assert.equal(r.status, 201, JSON.stringify(r.json));
    assert.equal(r.json.worker.running, true);
    const a = await api.settle(r.json.id);
    assert.equal(a.status, 'done', JSON.stringify(a));
    const thought = a.events.find((e) => e.type === 'thought');
    assert.match(thought.text, /^Tools: wiki_read, wiki_search\./, 'the agent was offered exactly the two read tools');
    const tools = a.events.filter((e) => e.type === 'tool');
    assert.deepEqual(tools.map((e) => [e.tool, e.status]), [['wiki_search', 'running'], ['wiki_search', 'done'], ['wiki_read', 'running'], ['wiki_read', 'done']]);
    const search = tools[1];
    assert.equal(search.args.query, 'what tray logon task called');
    assert.ok(search.count >= 1);
    assert.deepEqual(search.hits[0], { target: 'alpha', kind: 'page', title: 'Alpha', snippet: 'The tray logon task is called AgentWikiTray in Task Scheduler.' });
    assert.deepEqual({ ...tools[3], at: 0 }, { at: 0, type: 'tool', id: tools[3].id, tool: 'wiki_read', args: { target: 'alpha' }, status: 'done', target: 'alpha', kind: 'page', title: 'Alpha', chars: tools[3].chars });
    assert.equal(a.events.at(-1).type, 'answered');
    assert.match(a.result.answer, /AgentWikiTray/);
    assert.equal(a.result.found, true);
    assert.deepEqual(a.result.sources, [{ target: 'alpha', kind: 'page', title: 'Alpha', type: 'project', quote: 'The tray logon task is called AgentWikiTray in Task Scheduler.' }]);
    assert.equal(a.result.searches, 1);
    assert.equal(a.result.reads, 1);
    assert.ok(a.result.ms > 0 && a.result.model);
    assert.deepEqual(snapshot(W.wikiDir), before, 'asking changed nothing in the wiki (only .curator/asks)');

    // The request log ties the agent's tool calls to the question.
    const log = requestLog(W.logs);
    const calls = log.filter((l) => l.kind === 'tool' && l.ask === r.json.id);
    assert.deepEqual(calls.map((l) => [l.proc, l.tool, l.result]), [['ask', 'wiki_search', 'ok'], ['ask', 'wiki_read', 'ok']]);
    assert.ok(log.some((l) => l.kind === 'ask' && l.event === 'start' && l.ask === r.json.id && l.question === 'What is the tray logon task called?'));
    const done = log.find((l) => l.kind === 'ask' && l.event === 'done' && l.ask === r.json.id);
    assert.equal(done.result, 'ok');
    assert.deepEqual(done.sources, ['alpha']);

    // The agent runs isolated, like the curator: its own CODEX_HOME, no API key, no shell, one MCP server.
    const exec = fs.readFileSync(W.env.FAKE_CODEX_LOG, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)).filter((c) => c.args[0] === 'exec').at(-1);
    assert.match(exec.codexHome, /curator[\\/]codex-home$/);
    assert.equal(exec.apiKey, false);
    for (const f of ['shell_tool', 'unified_exec', 'plugins', 'apps', 'hooks']) assert.ok(exec.args.join(' ').includes(`--disable ${f}`), f);
    assert.ok(exec.args.includes('--ignore-user-config'));
    assert.ok(exec.args.some((x) => /^mcp_servers\.wiki\.args=\[(?:.*server\.mjs"|"serve"), "--read-only"\]$/.test(x)), 'the read-only server');
    assert.ok(exec.args.includes('model_reasoning_effort="low"'));
  });

  test('a follow-up carries the conversation; the list shows one thread with both turns', async () => {
    const first = (await api.req('GET', '/api/asks')).json.asks[0];
    const r = await api.ask('And where do request logs live?', first.id);
    assert.equal(r.status, 201);
    const a = await api.settle(r.json.id);
    assert.equal(a.status, 'done');
    assert.equal(a.parent, first.id);
    assert.deepEqual(a.result.sources.map((s) => s.target), ['beta']);
    const prompts = fs.readdirSync(W.env.FAKE_CODEX_PROMPTS).sort((x, y) => Number.parseInt(x) - Number.parseInt(y));
    const prompt = fs.readFileSync(path.join(W.env.FAKE_CODEX_PROMPTS, prompts.at(-1)), 'utf8');
    assert.match(prompt, /Earlier in this conversation[\s\S]*Q: What is the tray logon task called\?\nA: [^\n]*AgentWikiTray[\s\S]*<question>\nAnd where do request logs live\?\n<\/question>/);
    assert.match(prompt, /- alpha \[project\] Alpha - About Alpha/, 'the page index');
    const t = (await api.get(r.json.id, '&thread=1')).json;
    assert.deepEqual(t.thread.map((x) => [x.id, x.status]), [[first.id, 'done']]);
    const list = (await api.req('GET', '/api/asks')).json.asks;
    assert.equal(list.length, 1);
    assert.equal(list[0].id, r.json.id);
    assert.equal(list[0].root, first.id);
    assert.equal(list[0].turns, 2);
    assert.equal(list[0].first, 'What is the tray logon task called?');
    assert.match(list[0].preview, /logs/);
    assert.equal((await api.ask('More?', '2020-01-01_00-00-00-000-abcdef')).status, 400, 'an unknown parent');
  });

  test('the agent cannot write: wiki_log is not even offered, and nothing reaches the inbox', async () => {
    W.setMode('ask-write');
    const a = await api.settle((await api.ask('What is the tray logon task called?')).json.id);
    assert.equal(a.status, 'done');
    const write = a.events.find((e) => e.type === 'tool' && e.tool === 'wiki_log' && e.status !== 'running');
    assert.equal(write.status, 'error');
    assert.ok(!fs.existsSync(path.join(W.wikiDir, 'inbox')) || fs.readdirSync(path.join(W.wikiDir, 'inbox')).length === 0);
    assert.ok(!requestLog(W.logs).some((l) => l.tool === 'wiki_log'));
  });

  test('Stop: a running question is stopped and says so', async () => {
    W.setMode('normal,delay=1500');
    const id = (await api.ask('What is the tray logon task called?')).json.id;
    await api.settle(id, { until: (a) => a.status === 'running' && a.events.some((e) => e.type === 'tool') });
    const c = await api.cancel(id);
    assert.equal(c.status, 200);
    const a = await api.settle(id, { timeoutMs: 15_000 });
    assert.equal(a.status, 'cancelled');
    assert.equal(a.result.error, 'Stopped.');
    assert.ok(requestLog(W.logs).some((l) => l.kind === 'ask' && l.event === 'done' && l.ask === id && l.result === 'aborted'));
    assert.equal((await api.cancel('nope')).status, 400);
  });

  test('failures are explained: signed out, unusable output; questions it will not take', async () => {
    W.setMode('signed-out');
    let a = await api.settle((await api.ask('What is the tray logon task called?')).json.id);
    assert.equal(a.status, 'error');
    assert.equal(a.result.errorKind, 'signed_out');
    assert.match(a.result.error, /Sign in/);
    W.setMode('bad-json');
    a = await api.settle((await api.ask('What is the tray logon task called?')).json.id);
    assert.equal(a.status, 'error');
    assert.equal(a.result.errorKind, 'bad_output');
    W.setMode('normal');
    const secret = await api.ask(`Is ${SECRET} my key?`);
    assert.equal(secret.status, 400);
    assert.match(secret.json.error, /secret|key/i);
    assert.equal((await api.ask('   ')).status, 400);
    assert.equal((await api.ask('x'.repeat(2001))).status, 400);
    assert.equal((await api.get('2020-01-01_00-00-00-000-abcdef')).status, 404);
    assert.equal((await api.get('../etc')).status, 400);
  });

  test('concurrent Rust questions never rewrite a schema another child is reading', { skip: process.env.AGENT_WIKI_IMPL !== 'rust' }, async () => {
    const calls = () => fs.readFileSync(W.env.FAKE_CODEX_LOG, 'utf8').split('\n').filter(Boolean).map(JSON.parse).filter(c => c.args[0] === 'exec');
    const before = calls().length;
    W.setMode('normal,delay=200');
    const replies = await Promise.all(['What is the tray logon task called?', 'Where do request logs live?'].map(q => api.ask(q)));
    assert.ok(replies.every(r => r.status === 201));
    const answers = await Promise.all(replies.map(r => api.settle(r.json.id)));
    assert.ok(answers.every(a => a.status === 'done'), JSON.stringify(answers));
    const schemas = calls().slice(before).map(c => c.args[c.args.indexOf('--output-schema') + 1]);
    assert.equal(schemas.length, 2);
    assert.equal(new Set(schemas).size, 2, 'simultaneous model children must not share a writable schema');
    assert.ok(schemas.every(p => !fs.existsSync(p)), 'completed requests clean up their schemas');
    W.setMode('normal');
  });

  test('left behind: a question nobody picked up expires; one whose worker died is marked interrupted', async () => {
    await curator.stop();
    curator = null;
    for (let i = 0; i < 50 && (await api.req('GET', '/api/status')).json.ask?.running; i++) await sleep(100);
    assert.equal((await api.ask('Anyone there?')).status, 503, 'a stopped worker says so in its heartbeat');
    // Written by hand the way the service writes them, dated today so the two-week sweep keeps them.
    const d = new Date();
    const today = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
    const asks = path.join(W.wikiDir, '.curator', 'asks');
    const put = (id, extra = {}) => {
      fs.mkdirSync(path.join(asks, id), { recursive: true });
      fs.writeFileSync(path.join(asks, id, 'ask.json'), JSON.stringify({ id, question: `${id}?`, parent: null, createdAt: new Date(Date.now() - 3600_000).toISOString(), ...extra }));
    };
    const old = `${today}_00-00-00-000-aaaaaa`;
    put(old); // queued an hour ago, while no curator ran (they wait 10 minutes)
    const dead = `${today}_00-00-01-000-bbbbbb`;
    put(dead, { createdAt: new Date().toISOString() });
    fs.writeFileSync(path.join(asks, dead, 'claim.json'), JSON.stringify({ pid: 999999, startedAt: new Date().toISOString() }));
    const past = new Date(Date.now() - 10 * 60_000);
    fs.utimesSync(path.join(asks, dead, 'claim.json'), past, past); // its worker stopped beating 10 minutes ago
    assert.equal((await api.get(old)).json.status, 'queued');
    assert.equal((await api.get(dead)).json.status, 'stalled');
    curator = startCurator(W.env);
    await api.workerUp();
    const expired = await api.settle(old, { timeoutMs: 10_000 });
    assert.equal(expired.result.errorKind, 'expired');
    assert.match(expired.result.error, /Ask again/);
    assert.equal((await api.settle(dead, { timeoutMs: 10_000, until: (a) => a.status === 'error' })).result.errorKind, 'interrupted');
  });
});
