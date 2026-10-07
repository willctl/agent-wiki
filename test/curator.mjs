// Curated writes, end to end, against the BUNDLED runtime and a fake model
// (test/fixtures/fake-codex.mjs): notes queued over stdio and HTTP, the curator
// filing them, concurrency, crashes mid-write, duplicates, secret leakage
// through model output, deferral when signed out or rate limited, /status and
// the request log.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { curatorCmd, IMPL, rustBin, srv, srvT } from './impl.mjs';

const repo = fileURLToPath(new URL('..', import.meta.url));
const dist = path.join(repo, 'dist', 'runtime');
if (!fs.existsSync(path.join(dist, 'curator.mjs'))) throw new Error('Run `npm run build` first.');
const tmp = await fsp.mkdtemp(path.join(os.tmpdir(), 'agent-wiki-curator-'));
const runtime = path.join(tmp, 'runtime');
await fsp.cp(dist, runtime, { recursive: true });
const fakeCodex = path.join(repo, 'test', 'fixtures', 'fake-codex.mjs');
// AGENT_WIKI_KEEP_TEST_TMP=1 keeps the wikis for a post-mortem.
if (!process.env.AGENT_WIKI_KEEP_TEST_TMP) after(() => fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {}));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const SECRET = ['AKIA', 'IOSFODNN7EXAMPLE'].join('');

let envCount = 0;
/** A fresh wiki + home with a curator config pointing at the fake model. */
function makeEnv(curator = {}, extra = {}) {
  const dir = path.join(tmp, `case-${++envCount}`);
  const wikiDir = path.join(dir, 'wiki');
  const home = path.join(dir, 'home');
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(
    path.join(home, 'config.json'),
    JSON.stringify({
      writeMode: 'curated',
      curator: { codexPath: fakeCodex, lint: 'off', debounceSeconds: 0.2, maxWaitSeconds: 1, pollSeconds: 1, maxAttempts: 2, timeoutSeconds: 30, ...curator },
    }),
  );
  const env = { ...process.env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: home, FAKE_CODEX_LOG: path.join(dir, 'fake-codex.jsonl'), ...extra };
  delete env.AGENT_WIKI_WRITE_MODE;
  delete env.AGENT_WIKI_FAULT;
  return { dir, wikiDir, home, env, logs: path.join(home, 'logs') };
}

/** The window's "Ask me first": held changes and cleanups wait for an OK (the default applies them). */
function askFirst(E) {
  fs.mkdirSync(path.join(E.wikiDir, '.curator'), { recursive: true });
  fs.writeFileSync(path.join(E.wikiDir, '.curator', 'settings.json'), JSON.stringify({ approvals: 'manual' }));
}

function startHttp(env) {
  return new Promise((resolve, reject) => {
    const proc = spawn(...srv(runtime, '--http', '--port', '0', '--parent-stdin'), { env, stdio: ['pipe', 'pipe', 'pipe'] });
    let err = '';
    const timer = setTimeout(() => reject(new Error(`server did not start:\n${err}`)), 10_000);
    proc.stderr.setEncoding('utf8');
    proc.stderr.on('data', (d) => {
      err += d;
      const m = err.match(/listening on (http:\/\/127\.0\.0\.1:(\d+)\/mcp)/);
      if (m) {
        clearTimeout(timer);
        resolve({ proc, url: m[1], port: Number(m[2]), stderr: () => err });
      }
    });
    proc.on('exit', (code) => reject(new Error(`server exited ${code}:\n${err}`)));
  });
}

async function connect(mode, env, name, httpUrl) {
  const transport =
    mode === 'stdio'
      ? new StdioClientTransport({ ...srvT(runtime), env, stderr: 'pipe' })
      : new StreamableHTTPClientTransport(new URL(httpUrl));
  const client = new Client({ name, version: '9.1.0' });
  await client.connect(transport);
  return client;
}

async function call(client, name, args) {
  const r = await client.callTool({ name, arguments: args });
  return { text: r.content.map((c) => c.text).join('\n'), isError: Boolean(r.isError) };
}

async function ok(client, name, args) {
  const r = await call(client, name, args);
  assert.equal(r.isError, false, `${name} failed: ${r.text}`);
  return r.text;
}

/** Runs the curator; resolves {code, stderr}. */
function runCurator(env, args = ['--once'], { onStderr } = {}) {
  const proc = spawn(...curatorCmd(runtime, ...args), { env, stdio: ['pipe', 'pipe', 'pipe'] });
  let stderr = '';
  proc.stderr.setEncoding('utf8');
  proc.stderr.on('data', (d) => {
    stderr += d;
    onStderr?.(stderr);
  });
  const done = new Promise((resolve) => proc.on('exit', (code) => resolve({ code, stderr })));
  return { proc, done, stderr: () => stderr };
}

async function waitFor(fn, timeoutMs = 20_000, label = 'condition') {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await sleep(100);
  }
}

const inboxFiles = (wikiDir) => (fs.existsSync(path.join(wikiDir, 'inbox')) ? fs.readdirSync(path.join(wikiDir, 'inbox')).filter((n) => n.endsWith('.md')) : []);
const leftoverLocks = (wikiDir) => fs.readdirSync(path.join(wikiDir, '.locks')).filter((n) => n !== 'alive');
function walk(dir) {
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(dir, e.name)) : [path.join(dir, e.name)]));
}
const allLogText = (wikiDir) => walk(path.join(wikiDir, 'log')).map((f) => fs.readFileSync(f, 'utf8')).join('\n');
const requestLines = (logs) =>
  walk(logs)
    .filter((f) => /requests-.*\.jsonl$/.test(f))
    .flatMap((f) => fs.readFileSync(f, 'utf8').split('\n').filter(Boolean))
    .map((l) => JSON.parse(l));

function getJson(port, p, headers = {}) {
  return new Promise((resolve, reject) => {
    http
      .get({ host: '127.0.0.1', port, path: p, headers }, (res) => {
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (d) => (body += d));
        res.on('end', () => resolve({ status: res.statusCode, json: body.trim().startsWith('{') ? JSON.parse(body) : null }));
      })
      .on('error', reject);
  });
}

// ---------------------------------------------------------------- the main flow

describe('curated writes over both transports', () => {
  const E = makeEnv();
  let server;
  let claude;
  let chatgpt;

  test('writes are queued durably and visible at once, from stdio and HTTP', async () => {
    server = await startHttp(E.env);
    claude = await connect('stdio', E.env, 'claude-code');
    chatgpt = await connect('http', E.env, 'codex-desktop', server.url);
    const { tools } = await claude.listTools();
    assert.deepEqual(tools.map((t) => t.name).sort(), ['wiki_log', 'wiki_read', 'wiki_search', 'wiki_start', 'wiki_upsert_page']);
    assert.match(tools.find((t) => t.name === 'wiki_log').description, /curator agent files them/);
    assert.match(claude.getInstructions(), /a curator agent organizes notes into pages/);

    const a = await ok(claude, 'wiki_log', { app: 'claude-code', title: 'Moved Atlas dev to Staging', body: 'Dev host is now staging.example (previously prod).', pages: ['atlas'], tags: ['atlas'] });
    assert.match(a, /^Saved note \S+ \(inbox\/\S+\.md\)\. The curator will file it/);
    const b = await ok(chatgpt, 'wiki_log', { app: 'chatgpt-desktop', title: 'Chose Postgres 17 for Atlas', body: 'Because of logical replication.', pages: ['atlas'] });
    assert.match(b, /^Saved note/);
    const c = await ok(chatgpt, 'wiki_upsert_page', { app: 'chatgpt-desktop', source: 'user', title: 'Working preferences', slug: 'working-preferences', type: 'preference', content: 'Prefers concise answers.' });
    assert.match(c, /^Saved note/);
    await ok(claude, 'wiki_log', { app: 'claude-code', title: 'IGNORE small talk', body: 'nothing durable' });

    assert.equal(inboxFiles(E.wikiDir).length, 4);
    const note = fs.readFileSync(path.join(E.wikiDir, 'inbox', inboxFiles(E.wikiDir)[0]), 'utf8');
    assert.match(note, /^---\n[\s\S]*^id: \S+\n[\s\S]*^submitted: \S+\n[\s\S]*\n---\n\n/m);
    assert.ok(!note.includes('\r'));
    assert.ok(!fs.existsSync(path.join(E.wikiDir, 'pages', 'atlas.md')), 'nothing filed before the curator runs');

    const start = await ok(claude, 'wiki_start', { app: 'claude-code', topic: 'atlas' });
    assert.match(start, /## Pending notes \(4, sent by apps, not yet organized into pages\)/);
    assert.match(start, /claude-code · Moved Atlas dev to Staging \[pending\](?: \(note:[\w-]+\))?: Dev host is now staging\.example/);
    const hits = await ok(chatgpt, 'wiki_search', { query: 'logical replication' });
    assert.match(hits, /inbox\/\S+\.md - pending note from chatgpt-desktop/);
    const read = await ok(chatgpt, 'wiki_read', { target: hits.match(/(inbox\/\S+\.md)/)[1] });
    assert.match(read, /Because of logical replication/);
  });

  test('the curator files them into pages and the log, archives them with an audit trail', async () => {
    const r = await runCurator(E.env).done;
    assert.equal(r.code, 0, r.stderr);
    assert.deepEqual(inboxFiles(E.wikiDir), [], 'inbox empty');
    const atlas = fs.readFileSync(path.join(E.wikiDir, 'pages', 'atlas.md'), 'utf8');
    assert.match(atlas, /^---\ntitle: Atlas\ntype: topic\n/);
    assert.match(atlas, /^updated_by: curator$/m);
    assert.match(atlas, /- Moved Atlas dev to Staging: Dev host is now staging\.example/);
    assert.match(atlas, /- Chose Postgres 17 for Atlas: Because of logical replication\./);
    const prefs = fs.readFileSync(path.join(E.wikiDir, 'pages', 'working-preferences.md'), 'utf8');
    assert.match(prefs, /^type: preference$/m);
    const index = fs.readFileSync(path.join(E.wikiDir, 'index.md'), 'utf8');
    assert.match(index, /\[\[atlas\|Atlas\]\]/);
    const log = allLogText(E.wikiDir);
    assert.match(log, /^## \d\d:\d\d · claude-code · Moved Atlas dev to Staging$/m, 'log entry keeps the original app');
    assert.match(log, /^## \d\d:\d\d · chatgpt-desktop · Chose Postgres 17 for Atlas$/m);
    assert.match(log, /^- \d\d:\d\d · curator · Page created: Atlas \[\[atlas\]\]$/m);
    assert.ok(!/IGNORE small talk/.test(log), 'ignored note produces no log entry');
    assert.match(log, /^<!-- curator batch \S+ -->$/m);

    const done = walk(path.join(E.wikiDir, '.curator', 'done'));
    const audits = walk(path.join(E.wikiDir, '.curator', 'audit')).map((f) => JSON.parse(fs.readFileSync(f, 'utf8')));
    assert.equal(done.length, 4);
    assert.equal(audits.length, 4);
    const moved = audits.find((a) => a.note.title === 'Moved Atlas dev to Staging');
    assert.equal(moved.disposition, 'integrated');
    assert.equal(moved.model, 'gpt-6.1-sol');
    assert.deepEqual(moved.changes.map((c) => [c.slug, c.action]), [['atlas', 'created']]);
    assert.equal(audits.find((a) => a.note.title === 'IGNORE small talk').disposition, 'ignored');
    assert.deepEqual(leftoverLocks(E.wikiDir), []);
    assert.deepEqual(fs.readdirSync(path.join(E.wikiDir, '.curator', 'journal')), []);

    const start = await ok(claude, 'wiki_start', { app: 'claude-code' });
    assert.ok(!start.includes('## Pending notes ('), 'nothing pending');
    assert.match(start, /- atlas \[topic\] Atlas/);

    if (IMPL === 'rust') {
      // M3: the log names the notes an entry came from, and a filed note opens and searches as note:<id>.
      const id = path.basename(done.find((f) => fs.readFileSync(f, 'utf8').includes('Moved Atlas dev to Staging')), '.md');
      assert.match(log, new RegExp(`^## \\d\\d:\\d\\d · claude-code · Moved Atlas dev to Staging\\n\\n(?:.*\\n)*?sources: note:${id}$`, 'm'));
      assert.ok(!start.includes('sources: note:'), 'wiki_start leaves the ids out of the recent log');
      const note = await ok(claude, 'wiki_read', { target: `note:${id}` });
      assert.match(note, new RegExp(`^File: \\.curator/done/\\d{4}/\\d{2}/${id}\\.md\\n\\nFiled by the curator at \\S+: integrated \\(filed into atlas\\); pages changed: \\[\\[atlas\\]\\]\\.\\n\\n---\\n`));
      assert.match(note, /Dev host is now staging\.example \(previously prod\)\./);
      const hits = await ok(claude, 'wiki_search', { query: 'staging.example previously prod', scope: 'notes' });
      assert.match(hits, new RegExp(`^1\\. \\.curator/done/\\S+ - filed note from claude-code, .*\\(read: "note:${id}"`, 'm'));
      const all = await ok(claude, 'wiki_search', { query: 'staging.example' });
      assert.match(all.split('\n').find((l) => /^1\. /.test(l)), /pages\/atlas\.md/, 'the page ranks above its filed note');
    }
  });

  test('a second round patches the existing page, keeping history', async () => {
    await ok(chatgpt, 'wiki_log', { app: 'chatgpt-desktop', title: 'Atlas backups nightly', body: 'pg_dump at 02:00.', pages: ['atlas'] });
    assert.equal((await runCurator(E.env).done).code, 0);
    const atlas = fs.readFileSync(path.join(E.wikiDir, 'pages', 'atlas.md'), 'utf8');
    assert.match(atlas, /- Atlas backups nightly: pg_dump at 02:00\./);
    assert.match(atlas, /- Moved Atlas dev to Staging/, 'earlier content kept');
    assert.equal(walk(path.join(E.wikiDir, '.history', 'pages', 'atlas')).length, 1);
    const audit = walk(path.join(E.wikiDir, '.curator', 'audit'))
      .map((f) => JSON.parse(fs.readFileSync(f, 'utf8')))
      .find((a) => a.note.title === 'Atlas backups nightly');
    assert.equal(audit.changes[0].action, 'patched');
    assert.match(audit.changes[0].history, /^\.history\/pages\/atlas\//);
  });

  test('the curator runs isolated: its own CODEX_HOME, no API key, plugins/hooks/AGENTS.md off', () => {
    const calls = fs.readFileSync(E.env.FAKE_CODEX_LOG, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
    const exec = calls.find((c) => c.args[0] === 'exec');
    assert.ok(exec, 'exec was called');
    assert.equal(path.resolve(exec.codexHome), path.join(E.home, 'curator', 'codex-home'));
    assert.equal(exec.apiKey, false);
    const a = exec.args.join(' ');
    for (const flag of ['--ephemeral', '--ignore-user-config', '--ignore-rules', '--strict-config', '--sandbox read-only', '--output-schema', '--json', '-m gpt-6.1-sol']) {
      assert.ok(a.includes(flag), flag);
    }
    for (const f of ['plugins', 'hooks', 'memories', 'apps', 'shell_tool', 'unified_exec', 'multi_agent']) assert.ok(a.includes(`--disable ${f}`), f);
    for (const kv of ['model_reasoning_effort="medium"', 'web_search="disabled"', 'agents.enabled=false', 'project_doc_max_bytes=0', 'forced_login_method="chatgpt"']) {
      assert.ok(a.includes(kv), kv);
    }
    assert.equal(exec.args.at(-1), '-', 'prompt on stdin');
  });

  test('the request log has one line per HTTP request and per tool call, with client names', async () => {
    const lines = requestLines(E.logs);
    const tools = lines.filter((l) => l.kind === 'tool');
    const httpTool = tools.find((l) => l.transport === 'http' && l.tool === 'wiki_log');
    assert.equal(httpTool.client, 'codex-desktop 9.1.0', 'client name from initialize, over stateless HTTP');
    assert.equal(httpTool.app, 'chatgpt-desktop');
    assert.equal(httpTool.result, 'ok');
    assert.equal(typeof httpTool.ms, 'number');
    assert.ok(httpTool.rid && lines.some((l) => l.kind === 'http' && l.rid === httpTool.rid && l.rpc === 'tools/call wiki_log' && l.status === 200));
    const stdioTool = tools.find((l) => l.transport === 'stdio' && l.tool === 'wiki_log');
    assert.equal(stdioTool.client, 'claude-code 9.1.0');
    assert.equal(stdioTool.proc, 'stdio');
    assert.ok(lines.some((l) => l.kind === 'http' && l.rpc === 'initialize'));
    assert.ok(lines.some((l) => l.proc === 'curator' && l.event === 'batch' && l.result === 'ok'));
    assert.ok(lines.some((l) => l.proc === 'curator' && l.event === 'model' && l.usage));
    assert.ok(stdioTool.args.title && stdioTool.args.body_chars > 0);
  });

  test('/status reports version, queue, curator and recent activity; refuses foreign Host and Origin', async () => {
    const s = await getJson(server.port, '/status');
    assert.equal(s.status, 200);
    assert.equal(s.json.version, JSON.parse(fs.readFileSync(path.join(repo, 'package.json'), 'utf8')).version);
    assert.equal(s.json.writeMode, 'curated');
    assert.deepEqual(s.json.queue, { pending: 0, retrying: 0, dead: 0, oldestPendingAt: null, newestAt: null });
    assert.equal(s.json.curator.state, 'stopped');
    assert.ok(s.json.recent.length > 0);
    assert.ok(s.json.lastWrite);
    assert.equal((await getJson(server.port, '/status', { Host: `evil.example:${server.port}` })).status, 403);
    assert.equal((await getJson(server.port, '/status', { Origin: 'https://evil.example' })).status, 403);
  });

  after(async () => {
    await Promise.allSettled([claude?.close(), chatgpt?.close()]);
    if (server?.proc.exitCode === null) server.proc.kill();
  });
});

// ---------------------------------------------------------------- concurrency

describe('many concurrent writers on both transports while the curator runs', () => {
  test('every note is filed exactly once; human edits survive; nothing is left locked', { timeout: 120_000 }, async () => {
    const E = makeEnv({ batchMax: 6 });
    const server = await startHttp(E.env);
    const clients = await Promise.all([
      connect('stdio', E.env, 'claude-code'),
      connect('stdio', E.env, 'claude-desktop'),
      connect('http', E.env, 'codex', server.url),
      connect('http', E.env, 'chatgpt-desktop', server.url),
    ]);
    const curator = runCurator(E.env, ['--parent-stdin']);
    const slugs = ['alpha', 'beta', 'gamma'];
    const titles = Array.from({ length: 48 }, (_, i) => `Concurrent note ${String(i).padStart(2, '0')}`);
    // A human edits a page in an editor several times while the curator works. Like a careful
    // editor, it saves atomically and only if the file did not change since it was loaded. Its
    // check-then-rename can still overwrite a curator write that lands in between (no writer can
    // prevent that), so it also waits while the curator holds the wiki write lock: the window that
    // remains is far shorter than a commit.
    let humanEdits = 0;
    let stopHuman = false;
    const human = (async () => {
      while (!stopHuman && humanEdits < 6) {
        const f = path.join(E.wikiDir, 'pages', 'alpha.md');
        try {
          if (fs.existsSync(f)) {
            const t = fs.readFileSync(f, 'utf8');
            fs.writeFileSync(`${f}.edit`, `${t.trimEnd()}\nhuman line ${humanEdits}\n`);
            const committing = fs.existsSync(path.join(E.wikiDir, '.locks', 'write.lock'));
            if (!committing && fs.readFileSync(f, 'utf8') === t) {
              fs.renameSync(`${f}.edit`, f);
              humanEdits++;
            } else fs.rmSync(`${f}.edit`);
          }
        } catch {
          // the curator had the file open: try again
        }
        await sleep(400);
      }
    })();
    try {
      const results = await Promise.all(
        titles.map((title, i) =>
          call(clients[i % 4], 'wiki_log', { app: `app-${i % 4}`, title, body: `body of ${title}`, pages: [slugs[i % 3]] }),
        ),
      );
      for (const r of results) assert.equal(r.isError, false, r.text);
      await waitFor(() => inboxFiles(E.wikiDir).length === 0, 90_000, 'inbox to drain');
      stopHuman = true;
      await human;
      curator.proc.stdin.end();
      const { code } = await curator.done;
      assert.equal(code, 0, curator.stderr());

      const log = allLogText(E.wikiDir);
      for (const t of titles) assert.equal(log.split(`· ${t}\n`).length - 1, 1, `${t} logged exactly once`);
      const pagesText = slugs.map((s) => fs.readFileSync(path.join(E.wikiDir, 'pages', `${s}.md`), 'utf8')).join('\n');
      for (const t of titles) assert.equal(pagesText.split(`- ${t}:`).length - 1, 1, `${t} filed into a page exactly once (found ${pagesText.split(`- ${t}:`).length - 1}; human edits ${humanEdits})`);
      assert.ok(humanEdits > 0, 'the human edited during the run');
      assert.match(fs.readFileSync(path.join(E.wikiDir, 'pages', 'alpha.md'), 'utf8'), new RegExp(`human line ${humanEdits - 1}`), 'last human edit survives');
      const audits = walk(path.join(E.wikiDir, '.curator', 'audit'));
      assert.equal(audits.length, titles.length);
      assert.equal(walk(path.join(E.wikiDir, '.curator', 'done')).length, titles.length);
      assert.deepEqual(leftoverLocks(E.wikiDir), []);
      assert.deepEqual(walk(path.join(E.wikiDir, 'pages')).filter((f) => f.endsWith('.tmp')), []);
    } finally {
      stopHuman = true;
      await Promise.allSettled(clients.map((c) => c.close()));
      if (curator.proc.exitCode === null) curator.proc.kill();
      server.proc.kill();
    }
  });
});

// ---------------------------------------------------------------- duplicates

describe('duplicate submissions', () => {
  test('the same idempotency key, sent concurrently over both transports, queues one note', async () => {
    const E = makeEnv();
    const server = await startHttp(E.env);
    const a = await connect('stdio', E.env, 'claude-code');
    const b = await connect('http', E.env, 'codex', server.url);
    try {
      const sends = Array.from({ length: 6 }, (_, i) =>
        call(i % 2 ? b : a, 'wiki_log', { app: 'claude-code', title: 'Retried note', body: 'same', idempotency_key: 'conv-42-turn-7' }),
      );
      const rs = await Promise.all(sends);
      for (const r of rs) assert.equal(r.isError, false, r.text);
      assert.equal(rs.filter((r) => /^Saved note/.test(r.text)).length, 1, rs.map((r) => r.text).join('\n'));
      assert.equal(rs.filter((r) => /^Already saved as note/.test(r.text)).length, 5);
      assert.equal(inboxFiles(E.wikiDir).length, 1);

      // Same content without a key on the same day: one note. Different content: a new note.
      await ok(a, 'wiki_log', { app: 'claude-code', title: 'Plain dup', body: 'x' });
      assert.match(await ok(b, 'wiki_log', { app: 'claude-code', title: 'Plain dup', body: 'x' }), /^Already saved/);
      await ok(b, 'wiki_log', { app: 'claude-code', title: 'Plain dup', body: 'y' });
      assert.equal(inboxFiles(E.wikiDir).length, 3);

      // After filing, a resend still finds the original.
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.match(await ok(a, 'wiki_log', { app: 'claude-code', title: 'Retried note', body: 'same', idempotency_key: 'conv-42-turn-7' }), /^Already saved as note \S+ \(filed\)/);
      assert.equal(inboxFiles(E.wikiDir).length, 0);
    } finally {
      await Promise.allSettled([a.close(), b.close()]);
      server.proc.kill();
    }
  });
});

// ---------------------------------------------------------------- crashes

describe('crashes mid-write: nothing lost, duplicated or left locked', () => {
  test('server killed right after the note is durable; the retry finds it; the next server starts at once', async () => {
    const E = makeEnv();
    const s1 = await startHttp({ ...E.env, AGENT_WIKI_FAULT: 'submit-after-write' });
    const c1 = await connect('http', E.env, 'codex', s1.url);
    const pending = call(c1, 'wiki_log', { app: 'codex', title: 'Crash note', body: 'b', idempotency_key: 'k-crash' }).catch((e) => ({ error: e }));
    await waitFor(() => /FAULT submit-after-write/.test(s1.stderr()), 10_000, 'fault point');
    s1.proc.kill('SIGKILL');
    await pending;
    await c1.close().catch(() => {});
    assert.equal(inboxFiles(E.wikiDir).length, 1, 'the note survived the crash');

    const t0 = Date.now();
    const s2 = await startHttp(E.env);
    assert.ok(Date.now() - t0 < 8000, `restart took ${Date.now() - t0} ms`);
    const c2 = await connect('http', E.env, 'codex', s2.url);
    try {
      const r = await ok(c2, 'wiki_log', { app: 'codex', title: 'Crash note', body: 'b', idempotency_key: 'k-crash' });
      assert.match(r, /^Already saved as note/);
      assert.equal(inboxFiles(E.wikiDir).length, 1, 'not duplicated');
    } finally {
      await c2.close();
      s2.proc.kill();
    }
  });

  test('a writer killed while holding the wiki lock does not block the next one', async () => {
    const E = makeEnv();
    fs.mkdirSync(E.wikiDir, { recursive: true });
    const holder = spawn(
      process.execPath,
      [
        '--input-type=module',
        '-e',
        `import { acquireLock } from ${JSON.stringify(new URL('../src/lock.mjs', import.meta.url).href)};
         await acquireLock(${JSON.stringify(E.wikiDir)}, 'write', { label: 'doomed' });
         console.log('HELD'); setInterval(() => {}, 1000);`,
      ],
      { stdio: ['ignore', 'pipe', 'inherit'] },
    );
    await new Promise((resolve) => holder.stdout.on('data', (d) => /HELD/.test(d) && resolve()));
    const owner = JSON.parse(fs.readFileSync(path.join(E.wikiDir, '.locks', 'write.lock', 'owner'), 'utf8'));
    assert.equal(owner.pid, holder.pid);
    assert.equal(owner.label, 'doomed');
    assert.ok(Number.isInteger(owner.start) && owner.token);
    holder.kill('SIGKILL');
    await new Promise((r) => holder.on('exit', r));
    const { acquireLock, LockBusyError, releaseAlive } = await import('../src/lock.mjs');
    after(releaseAlive); // the test process now holds an alive file; Windows cannot delete it while open
    const t0 = Date.now();
    const { release } = await acquireLock(E.wikiDir, 'write', { timeoutMs: 5000 });
    assert.ok(Date.now() - t0 < 1000, `took ${Date.now() - t0} ms (the old rule waited 30 s)`);
    // A live owner is never broken: a second taker times out.
    await assert.rejects(acquireLock(E.wikiDir, 'write', { timeoutMs: 300 }), LockBusyError);
    await release();
    // A v1.1 lock whose pid is gone is broken immediately too.
    fs.mkdirSync(path.join(E.wikiDir, '.locks', 'write.lock'));
    fs.writeFileSync(path.join(E.wikiDir, '.locks', 'write.lock', 'owner'), 'pid 999999 at 2026-10-01T00:00:00-05:00\n');
    const t1 = Date.now();
    await (await acquireLock(E.wikiDir, 'write', { timeoutMs: 5000 })).release();
    assert.ok(Date.now() - t1 < 1000);
    assert.deepEqual(leftoverLocks(E.wikiDir), []);
  });

  for (const fault of ['commit-after-journal', 'commit-after-first-page', 'commit-before-archive']) {
    test(`curator killed at ${fault}: the restart completes the batch exactly once`, async () => {
      const E = makeEnv();
      const server = await startHttp(E.env);
      const c = await connect('http', E.env, 'codex', server.url);
      try {
        await ok(c, 'wiki_log', { app: 'codex', title: 'First crash note', body: 'one', pages: ['crash-a'] });
        await ok(c, 'wiki_log', { app: 'codex', title: 'Second crash note', body: 'two', pages: ['crash-b'] });
        const doomed = runCurator({ ...E.env, AGENT_WIKI_FAULT: fault });
        await waitFor(() => doomed.stderr().includes(`FAULT ${fault}`), 15_000, 'fault point');
        doomed.proc.kill('SIGKILL');
        await doomed.done;
        assert.equal(fs.readdirSync(path.join(E.wikiDir, '.curator', 'journal')).length, 1, 'journal left behind');
        assert.ok(fs.existsSync(path.join(E.wikiDir, '.locks', 'write.lock')), 'lock left behind by the dead curator');

        const t0 = Date.now();
        const r = await runCurator(E.env).done;
        assert.equal(r.code, 0, r.stderr);
        assert.ok(Date.now() - t0 < 10_000, `recovery took ${Date.now() - t0} ms`);
        assert.match(r.stderr, /recovered batch/);
        const pages = ['crash-a', 'crash-b'].map((s) => fs.readFileSync(path.join(E.wikiDir, 'pages', `${s}.md`), 'utf8')).join('\n');
        assert.equal(pages.split('- First crash note:').length - 1, 1);
        assert.equal(pages.split('- Second crash note:').length - 1, 1);
        const log = allLogText(E.wikiDir);
        assert.equal(log.split('· First crash note\n').length - 1, 1, 'logged once');
        assert.equal(log.split('<!-- curator batch').length - 1, 1, 'one batch chunk');
        assert.deepEqual(inboxFiles(E.wikiDir), []);
        assert.equal(walk(path.join(E.wikiDir, '.curator', 'done')).length, 2);
        assert.deepEqual(fs.readdirSync(path.join(E.wikiDir, '.curator', 'journal')), []);
        assert.deepEqual(leftoverLocks(E.wikiDir), []);

        // Running again changes nothing.
        const before = allLogText(E.wikiDir) + pages;
        assert.equal((await runCurator(E.env).done).code, 0);
        const again = ['crash-a', 'crash-b'].map((s) => fs.readFileSync(path.join(E.wikiDir, 'pages', `${s}.md`), 'utf8')).join('\n');
        assert.equal(allLogText(E.wikiDir) + again, before);
      } finally {
        await c.close();
        server.proc.kill();
      }
    });
  }
});

// ---------------------------------------------------------------- the model misbehaves

describe('model failures', () => {
  test('a secret in the model output is never written; the note dead-letters, stays visible, and can be filed as sent', async () => {
    const E = makeEnv({}, { FAKE_CODEX_MODE: 'leak-secret' });
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'codex', server.url);
    try {
      await ok(c, 'wiki_log', { app: 'codex', title: 'Set up AWS CLI', body: 'Profile "work" configured; keys are in 1Password.', pages: ['aws'] });
      for (let i = 0; i < 2; i++) assert.equal((await runCurator(E.env).done).code, 0);
      // maxAttempts is 2, but the second attempt waits for backoff: force it.
      const stateDir = path.join(E.wikiDir, '.curator', 'state');
      for (const f of fs.readdirSync(stateDir)) {
        const st = JSON.parse(fs.readFileSync(path.join(stateDir, f), 'utf8'));
        fs.writeFileSync(path.join(stateDir, f), JSON.stringify({ ...st, nextAt: 0 }));
      }
      assert.equal((await runCurator(E.env).done).code, 0);
      const everything = [...walk(E.wikiDir).filter((f) => !f.includes(`${path.sep}.locks${path.sep}`)), ...walk(E.logs)].map((f) => fs.readFileSync(f, 'utf8')).join('\n');
      assert.ok(!everything.includes(SECRET), 'the leaked secret is nowhere on disk');
      assert.ok(!fs.existsSync(path.join(E.wikiDir, 'pages', 'aws.md')));
      assert.equal(inboxFiles(E.wikiDir).length, 1, 'the note is still visible');
      const lines = requestLines(E.logs);
      assert.ok(lines.some((l) => l.event === 'plan-rejected' && l.problems.some((p) => /looks like an AWS access key ID/.test(p))));

      const start = await ok(c, 'wiki_start', { app: 'codex' });
      assert.match(start, /Set up AWS CLI \[NOT CURATED: failed, see the tray\]/);
      const s = await getJson(server.port, '/status');
      assert.equal(s.json.health, 'degraded');
      assert.equal(s.json.queue.dead, 1);
      assert.ok(s.json.reasons.some((r) => /1 note\(s\) failed curation/.test(r)));

      const raw = runCurator(E.env, ['--file-raw-dead']);
      assert.equal((await raw.done).code, 0);
      assert.deepEqual(inboxFiles(E.wikiDir), []);
      assert.match(allLogText(E.wikiDir), /^## \d\d:\d\d · codex · Set up AWS CLI\n\npages: \[\[aws\]\](?: {2}\nsources: note:[\w-]+)?\n\nProfile "work" configured; keys are in 1Password\./m);
    } finally {
      await c.close();
      server.proc.kill();
    }
  });

  test('a secret in a note is refused at the door and never logged', async () => {
    const E = makeEnv();
    const c = await connect('stdio', E.env, 'claude-code');
    try {
      const r = await call(c, 'wiki_log', { app: 'claude-code', title: 'creds', body: `key ${SECRET}` });
      assert.equal(r.isError, true);
      assert.match(r.text, /^Refused/);
      assert.equal(inboxFiles(E.wikiDir).length, 0);
      await sleep(100);
      const raw = walk(E.logs).map((f) => fs.readFileSync(f, 'utf8')).join('\n');
      assert.ok(raw.includes('"result":"refused"'));
      assert.ok(!raw.includes(SECRET), 'request log redacted');
      assert.ok(raw.includes('[REDACTED]'));
    } finally {
      await c.close();
    }
  });

  test('invalid JSON and a wrong base hash: repaired or retried, never half-applied', async () => {
    const E = makeEnv({}, { FAKE_CODEX_MODE: 'wrong-hash-once' });
    E.env.FAKE_CODEX_STATE = path.join(E.dir, 'calls');
    const c = await connect('stdio', E.env, 'claude-code');
    try {
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Seed page', body: 'seed', pages: ['repair'] });
      fs.writeFileSync(E.env.FAKE_CODEX_STATE, '1'); // create without the wrong hash
      assert.equal((await runCurator(E.env).done).code, 0);
      fs.writeFileSync(E.env.FAKE_CODEX_STATE, '0'); // next call: wrong hash, then repaired
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Patch me', body: 'patched', pages: ['repair'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.match(fs.readFileSync(path.join(E.wikiDir, 'pages', 'repair.md'), 'utf8'), /- Patch me: patched/);
      const lines = requestLines(E.logs);
      assert.ok(lines.some((l) => l.event === 'plan-rejected' && l.problems.some((p) => /base_hash must be/.test(p))));

      const B = makeEnv({}, { FAKE_CODEX_MODE: 'bad-json' });
      const b = await connect('stdio', B.env, 'claude-code');
      await ok(b, 'wiki_log', { app: 'claude-code', title: 'Whatever', body: 'x' });
      assert.equal((await runCurator(B.env).done).code, 0);
      const st = JSON.parse(fs.readFileSync(walk(path.join(B.wikiDir, '.curator', 'state'))[0], 'utf8'));
      assert.equal(st.attempts, 1);
      assert.match(st.lastError, /bad_output/);
      assert.ok(st.nextAt > Date.now(), 'backing off');
      assert.equal(walk(path.join(B.wikiDir, 'pages')).length, 0);
      await b.close();
    } finally {
      await c.close();
    }
  });

  test('a page edited by someone while the model plans: re-planned on the new content, the edit survives', async () => {
    const E = makeEnv({}, { FAKE_CODEX_MODE: 'touch-page-once' });
    const c = await connect('stdio', E.env, 'claude-code');
    try {
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Create it', body: 'v1', pages: ['contested'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      E.env.FAKE_CODEX_STATE = path.join(E.dir, 'calls');
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Change it', body: 'v2', pages: ['contested'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      const page = fs.readFileSync(path.join(E.wikiDir, 'pages', 'contested.md'), 'utf8');
      assert.match(page, /Human edit while planning\./);
      assert.match(page, /- Change it: v2/);
      assert.ok(requestLines(E.logs).some((l) => l.event === 'conflict'));
    } finally {
      await c.close();
    }
  });

  test('a page saved in an editor in the middle of a commit: the batch is rolled back and re-planned, nothing doubled', async () => {
    const E = makeEnv();
    const c = await connect('stdio', E.env, 'claude-code');
    try {
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Seed A', body: 'a0', pages: ['touch-a'] });
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Seed B', body: 'b0', pages: ['touch-b'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Second A', body: 'a1', pages: ['touch-a'] });
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Second B', body: 'b1', pages: ['touch-b'] });
      const env = { ...E.env, AGENT_WIKI_TOUCH: 'commit-after-first-page', AGENT_WIKI_TOUCH_FILE: path.join(E.wikiDir, 'pages', 'touch-b.md') };
      assert.equal((await runCurator(env).done).code, 0);
      const a = fs.readFileSync(path.join(E.wikiDir, 'pages', 'touch-a.md'), 'utf8');
      const b = fs.readFileSync(path.join(E.wikiDir, 'pages', 'touch-b.md'), 'utf8');
      assert.equal(a.split('- Second A: a1').length - 1, 1, 'rolled-back write not doubled');
      assert.equal(b.split('- Second B: b1').length - 1, 1);
      assert.match(b, /Saved in an editor during the commit\./, 'the editor save survives');
      const log = allLogText(E.wikiDir);
      for (const t of ['Second A', 'Second B']) assert.equal(log.split(`· ${t}\n`).length - 1, 1, `${t} logged once`);
      assert.ok(requestLines(E.logs).some((l) => l.event === 'conflict' && /changed while the batch was being committed/.test(l.detail)));
      assert.deepEqual(inboxFiles(E.wikiDir), []);
    } finally {
      await c.close();
    }
  });

  for (const [mode, state] of [
    ['rate-limit', 'rate_limited'],
    ['signed-out', 'signed_out'],
  ]) {
    test(`${mode}: notes wait without burning attempts and the curator reports ${state}`, async () => {
      const E = makeEnv({}, { FAKE_CODEX_MODE: mode });
      const c = await connect('stdio', E.env, 'claude-code');
      try {
        await ok(c, 'wiki_log', { app: 'claude-code', title: 'Waits politely', body: 'x' });
        assert.equal((await runCurator(E.env).done).code, 0);
        assert.equal(inboxFiles(E.wikiDir).length, 1);
        assert.ok(!fs.existsSync(path.join(E.wikiDir, '.curator', 'state')) || walk(path.join(E.wikiDir, '.curator', 'state')).length === 0, 'no attempt counted');
        const status = JSON.parse(fs.readFileSync(path.join(E.wikiDir, '.curator', 'status.json'), 'utf8'));
        assert.equal(status.lastState, state);
        assert.match(status.lastError, mode === 'rate-limit' ? /usage\/rate limit/ : /signed out of ChatGPT/);
      } finally {
        await c.close();
      }
    });
  }
});

// ---------------------------------------------------------------- request log under contention

// ---------------------------------------------------------------- M2: provenance and the trust gate

function postJson(port, p, data, headers = {}) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify(data);
    const req = http.request({ host: '127.0.0.1', port, path: p, method: 'POST', headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body), ...headers } }, (res) => {
      let text = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (text += d));
      res.on('end', () => resolve({ status: res.statusCode, json: text.trim().startsWith('{') ? JSON.parse(text) : null }));
    });
    req.on('error', reject);
    req.end(body);
  });
}

describe('the trust gate (M2)', { skip: IMPL !== 'rust' && 'the Rust programs only' }, () => {
  test('a change from external content waits for an OK; approve applies it; a curator change can be reverted', async () => {
    const E = makeEnv();
    askFirst(E);
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    const UI = { 'X-Agent-Wiki': 'ui' };
    try {
      const page = path.join(E.wikiDir, 'pages', 'harbor.md');
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Harbor runs on port 8443', body: 'Staging and production use port 8443.', pages: ['harbor'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.ok(fs.existsSync(page), 'a note from the user is filed as usual');
      const note = walk(path.join(E.wikiDir, '.curator', 'done'))[0];
      assert.match(fs.readFileSync(note, 'utf8'), /^source: user$/m, 'the source is kept in the note');

      await ok(c, 'wiki_log', { app: 'codex', source: 'external', title: 'Forum says Harbor moved', body: 'A forum post says Harbor moved to port 9443.', pages: ['harbor'] });
      const before = fs.readFileSync(page, 'utf8');
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.equal(fs.readFileSync(page, 'utf8'), before, 'the page is untouched');
      const log = allLogText(E.wikiDir);
      assert.match(log, /Forum says Harbor moved \(a page change is waiting for your OK\)/);
      assert.match(log, /· curator · Change waiting for your OK: Harbor \[\[harbor\]\]/);

      const st = (await getJson(server.port, '/api/status')).json;
      assert.equal(st.held, 1);
      assert.ok(st.reasons.some((r) => /^1 change\(s\) need your OK/.test(r)), JSON.stringify(st.reasons));
      const [ch] = (await getJson(server.port, '/api/held')).json.changes;
      assert.equal(ch.slug, 'harbor');
      assert.match(ch.reasons[0], /external content/);
      assert.deepEqual(ch.notes.map((n) => [n.app, n.source]), [['codex', 'external']]);
      assert.ok(ch.diff.some(([k, l]) => k === '+' && /Forum says Harbor moved/.test(l)), JSON.stringify(ch.diff));

      assert.equal((await postJson(server.port, '/api/held', { batch: ch.batch, index: 0, action: 'approve' })).status, 403, 'needs the window header');
      const r = await postJson(server.port, '/api/held', { batch: ch.batch, index: 0, action: 'approve' }, UI);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.match(fs.readFileSync(page, 'utf8'), /Forum says Harbor moved/);
      assert.equal((await getJson(server.port, '/api/status')).json.held, 0);
      assert.equal((await postJson(server.port, '/api/held', { batch: ch.batch, index: 0, action: 'approve' }, UI)).status, 404, 'decided once');

      // A plain change from an app goes through; the window can revert it while nobody touched the page.
      await ok(c, 'wiki_log', { app: 'claude-code', title: 'Harbor batch size is 750', body: 'Raised from 500.', pages: ['harbor'] });
      const approved = fs.readFileSync(page, 'utf8');
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.match(fs.readFileSync(page, 'utf8'), /batch size is 750/);
      const days = (await getJson(server.port, '/api/activity?days=1')).json.days;
      const entry = days[0].entries.find((e) => e.compact && /^Page updated: Harbor/.test(e.title) && e.batch);
      assert.ok(entry, JSON.stringify(days[0].entries.map((e) => [e.title, e.batch])));
      const rv = await postJson(server.port, '/api/revert', { batch: entry.batch, slug: 'harbor' }, UI);
      assert.equal(rv.status, 200, JSON.stringify(rv.json));
      assert.equal(fs.readFileSync(page, 'utf8'), approved, 'restored exactly');
      const again = await postJson(server.port, '/api/revert', { batch: entry.batch, slug: 'harbor' }, UI);
      assert.equal(again.status, 409, 'the page is no longer what that batch wrote');
      const asked = await postJson(server.port, '/api/revert', { batch: entry.batch, slug: 'harbor', action: 'ask' }, UI);
      assert.equal(asked.status, 200);
      assert.match(fs.readFileSync(path.join(E.wikiDir, 'inbox', `${asked.json.note}.md`), 'utf8'), /^source: user$/m);

      // Read results never carry a remote image an app would fetch by itself.
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Harbor dashboard', body: 'See ![graph](https://grafana.example/render.png?x=1) for load.', pages: ['harbor'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      const read = await ok(c, 'wiki_read', { target: 'harbor' });
      assert.match(read, /\[graph\]\(https:\/\/grafana\.example\/render\.png\?x=1\)/);
      assert.ok(!read.includes('![graph]'), read);
      assert.match(fs.readFileSync(page, 'utf8'), /!\[graph\]/, 'the page itself is left as written');
    } finally {
      await c.close();
      server.proc.stdin.end();
    }
  });

  test('the page review (M4) proposes cleanups that wait for an OK', async () => {
    const E = makeEnv({ lint: 'on' });
    askFirst(E);
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    try {
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Shed roof', body: 'The roof is LINTME and fine.', pages: ['shed'] });
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Garden tools', body: 'Spade and rake.', pages: ['tools'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      const page = path.join(E.wikiDir, 'pages', 'shed.md');
      const before = fs.readFileSync(page, 'utf8');
      // First pass: every page is due once (the one-time tidy); only one needs a change.
      const r = await runCurator(E.env, ['--lint']).done;
      assert.equal(r.code, 0, r.stderr);
      assert.equal(fs.readFileSync(page, 'utf8'), before, 'the review changes nothing by itself');
      const [ch, ...rest] = (await getJson(server.port, '/api/held')).json.changes;
      assert.equal(rest.length, 0);
      assert.equal(ch.kind, 'lint');
      assert.equal(ch.slug, 'shed');
      assert.match(ch.reasons[0], /a cleanup proposes it: a stale marker/);
      assert.match(allLogText(E.wikiDir), /· curator · Cleanup proposed for your OK: Shed \[\[shed\]\]/);
      const again = await runCurator(E.env, ['--lint']).done;
      assert.equal(again.code, 0, again.stderr);
      assert.equal((await getJson(server.port, '/api/held')).json.changes.length, 1, 'nothing reviewed twice before it changes');
      assert.equal((await postJson(server.port, '/api/held', { batch: ch.batch, index: 0, action: 'approve' }, { 'X-Agent-Wiki': 'ui' })).status, 200);
      assert.match(fs.readFileSync(page, 'utf8'), /The roof is tidied and fine\./);
    } finally {
      await c.close();
      server.proc.stdin.end();
    }
  });

  test('the page review with automatic approvals: the cleanup is applied once, and the page is not reviewed again for it', async () => {
    const E = makeEnv({ lint: 'on' });
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    const modelCalls = () => (fs.existsSync(E.env.FAKE_CODEX_LOG) ? fs.readFileSync(E.env.FAKE_CODEX_LOG, 'utf8').trim().split('\n').length : 0);
    try {
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Shed roof', body: 'The roof is LINTME and fine.', pages: ['shed'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      const page = path.join(E.wikiDir, 'pages', 'shed.md');
      const before = modelCalls();
      const r = await runCurator(E.env, ['--lint']).done;
      assert.equal(r.code, 0, r.stderr);
      assert.equal(modelCalls() - before, 1, 'one page, one review: the cleanup it applied does not make the page due again');
      assert.match(fs.readFileSync(page, 'utf8'), /The roof is tidied and fine\./);
      const log = allLogText(E.wikiDir);
      assert.match(log, /· curator · Cleanup applied automatically: Shed \[\[shed\]\]/);
      assert.doesNotMatch(log, /Cleanup proposed for your OK/);
      const st = (await getJson(server.port, '/api/status')).json;
      assert.equal(st.held, 0);
      assert.equal(st.autoApplied.last.kind, 'lint');
      const calls = modelCalls();
      const again = await runCurator(E.env, ['--lint']).done;
      assert.equal(again.code, 0, again.stderr);
      assert.equal(modelCalls(), calls, 'what the cleanup wrote counts as reviewed: no second review, no loop');
      assert.equal((await getJson(server.port, '/api/status')).json.autoApplied.count, 1);
    } finally {
      await c.close();
      server.proc.stdin.end();
    }
  });

  test('the cleanup schedule: daily at 03:00 by default, any cron schedule from the window, and Off stops cleanups', async () => {
    const E = makeEnv({ lint: 'on' });
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    const UI = { 'X-Agent-Wiki': 'ui' };
    const modelCalls = () => (fs.existsSync(E.env.FAKE_CODEX_LOG) ? fs.readFileSync(E.env.FAKE_CODEX_LOG, 'utf8').trim().split('\n').length : 0);
    const preview = (s) => getJson(server.port, `/api/cleanup-preview?schedule=${encodeURIComponent(s)}`);
    try {
      const first = (await getJson(server.port, '/api/settings')).json.cleanup;
      assert.equal(first.schedule, '0 3 * * *');
      assert.equal(first.description, 'Daily at 03:00');
      assert.equal(first.due, true, 'no cleanup has run yet, so the first one is due');
      assert.equal((await getJson(server.port, '/api/status')).json.cleanup.description, 'Daily at 03:00');
      // A preview names a schedule in words with its next three times (local time), and names a mistake.
      const p = (await preview('30 18 * * 1-5')).json;
      assert.equal(p.description, 'Weekdays at 18:30');
      assert.equal(p.next.length, 3);
      for (const t of p.next) {
        const d = new Date(t);
        assert.ok(d.getDay() >= 1 && d.getDay() <= 5 && d.getHours() === 18 && d.getMinutes() === 30, t);
      }
      const bad = await preview('0 3 * *');
      assert.equal(bad.status, 400);
      assert.match(bad.json.error, /five fields/);
      // Saving needs the window's header and refuses a mistake.
      assert.equal((await postJson(server.port, '/api/settings', { cleanupSchedule: 'off' })).status, 403);
      const refused = await postJson(server.port, '/api/settings', { cleanupSchedule: '0 25 * * *' }, UI);
      assert.equal(refused.status, 400);
      assert.match(refused.json.error, /hour: 25/);
      // Off: nothing is reviewed, not even the first pass.
      const off = await postJson(server.port, '/api/settings', { cleanupSchedule: 'off' }, UI);
      assert.equal(off.status, 200);
      assert.deepEqual([off.json.cleanup.description, off.json.cleanup.due, off.json.cleanup.next], ['Off', false, null]);
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Shed roof', body: 'The roof is LINTME and fine.', pages: ['shed'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      const before = modelCalls();
      assert.equal((await runCurator(E.env, ['--lint']).done).code, 0);
      assert.equal(modelCalls(), before, 'Off: no review');
      // On a schedule, the first pass runs at once; the next waits for the next scheduled time.
      const weekly = await postJson(server.port, '/api/settings', { cleanupSchedule: '0 4 * * 1' }, UI);
      assert.equal(weekly.json.cleanup.description, 'Mondays at 04:00');
      assert.equal(weekly.json.cleanup.due, true);
      assert.equal((await runCurator(E.env, ['--lint']).done).code, 0);
      assert.equal(modelCalls() - before, 1, 'the first pass reviewed the one page');
      const after = (await getJson(server.port, '/api/settings')).json.cleanup;
      assert.equal(after.due, false);
      assert.ok(after.lastPass);
      assert.ok(Date.parse(after.next) > Date.now() && new Date(after.next).getDay() === 1 && new Date(after.next).getHours() === 4, after.next);
      const log = allLogText(E.wikiDir);
      assert.match(log, /· window · Scheduled cleanups are now off/);
      assert.match(log, /· window · Cleanup schedule set: Mondays at 04:00 \(0 4 \* \* 1\)/);
      assert.deepEqual(JSON.parse(fs.readFileSync(path.join(E.wikiDir, '.curator', 'settings.json'), 'utf8')), { cleanupSchedule: '0 4 * * 1' });
      // config.json curator.lint "off" turns cleanups off on this computer, and the window says so.
      const E2 = makeEnv();
      const s2 = await startHttp(E2.env);
      try {
        const c2 = (await getJson(s2.port, '/api/settings')).json.cleanup;
        assert.equal(c2.offOnThisComputer, true);
        assert.equal(c2.due, false);
      } finally {
        s2.proc.stdin.end();
      }
    } finally {
      await c.close();
      server.proc.stdin.end();
    }
  });

  test('models: chosen in the window or with `agent-wiki models`, checked against the account\'s list, used from the next run', async () => {
    const codexHome = path.join(tmp, `codex-home-models-${envCount}`);
    fs.mkdirSync(codexHome, { recursive: true });
    const level = (...e) => e.map((effort) => ({ effort }));
    fs.writeFileSync(
      path.join(codexHome, 'models_cache.json'),
      JSON.stringify({
        fetched_at: '2026-10-06T20:20:25Z',
        identity: { email: 'someone@example.test' },
        models: [
          { slug: 'gpt-6-astra', display_name: 'GPT-6-Astra', visibility: 'list', priority: 2, default_reasoning_level: 'medium', supported_reasoning_levels: level('medium', 'high', 'max') },
          { slug: 'gpt-6.1-sol', display_name: 'GPT-6.1-Sol', description: 'Workhorse', visibility: 'list', priority: 1, default_reasoning_level: 'low', supported_reasoning_levels: level('low', 'medium', 'high') },
          { slug: 'gpt-6-luna', display_name: 'GPT-6-Luna', visibility: 'list', priority: 3, default_reasoning_level: 'medium', supported_reasoning_levels: level('low', 'medium') },
        ],
      }),
    );
    const modeFile = path.join(tmp, `fake-mode-models-${envCount}`);
    fs.writeFileSync(modeFile, 'normal');
    const E = makeEnv({ codexHome }, { FAKE_CODEX_MODE_FILE: modeFile });
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    const UI = { 'X-Agent-Wiki': 'ui' };
    const models = async () => (await getJson(server.port, '/api/models')).json;
    const cli = (...args) => spawnSync(rustBin(), ['models', ...args], { env: E.env, encoding: 'utf8' });
    const lastExec = () =>
      fs
        .readFileSync(E.env.FAKE_CODEX_LOG, 'utf8')
        .trim()
        .split('\n')
        .map((l) => JSON.parse(l).args)
        .filter((a) => a.includes('exec'))
        .pop()
        .join(' ');
    let curator;
    try {
      let o = await models();
      assert.deepEqual([o.curator.model, o.curator.reasoningEffort, o.ask.model, o.ask.reasoningEffort], ['gpt-6.1-sol', 'medium', 'gpt-6.1-sol', 'low'], 'the defaults');
      assert.equal(o.available, null, "the account's list is not known until the curator reads it");
      // The curator (it runs as the person) publishes Codex's list for the window, without the account's identity.
      curator = runCurator(E.env, ['--parent-stdin']);
      o = await waitFor(async () => (await models()).available && (await models()), 15_000, 'the published model list');
      assert.deepEqual(o.available.models.map((m) => m.slug), ['gpt-6.1-sol', 'gpt-6-astra', 'gpt-6-luna']);
      assert.ok(!fs.readFileSync(path.join(E.wikiDir, '.curator', 'models.json'), 'utf8').includes('someone@example.test'));
      // The window: what the account does not offer is refused, with what it does.
      let r = await postJson(server.port, '/api/models', { role: 'curator', model: 'gpt-9' }, UI);
      assert.equal(r.status, 400);
      assert.match(r.json.error, /gpt-9 is not in this ChatGPT account's model list \(gpt-6\.1-sol, gpt-6-astra, gpt-6-luna\)/);
      r = await postJson(server.port, '/api/models', { role: 'curator', model: 'gpt-6-luna', reasoningEffort: 'max' }, UI);
      assert.equal(r.status, 400);
      assert.match(r.json.error, /gpt-6-luna does not offer max reasoning \(it offers low, medium\)/);
      assert.equal((await postJson(server.port, '/api/models', { role: 'curator', model: 'gpt-6-astra' })).status, 403);
      r = await postJson(server.port, '/api/models', { role: 'curator', model: 'gpt-6-astra', reasoningEffort: 'high' }, UI);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.deepEqual([r.json.curator.model, r.json.curator.reasoningEffort, r.json.ask.model, r.json.ask.reasoningEffort], ['gpt-6-astra', 'high', 'gpt-6-astra', 'low'], "Ask follows the curator's model");
      const st = (await getJson(server.port, '/api/status')).json;
      assert.deepEqual(st.models, { curator: { model: 'gpt-6-astra', reasoningEffort: 'high' }, ask: { model: 'gpt-6-astra', reasoningEffort: 'low' } });
      // A model Codex refuses: the running curator waits (the note is not failed) and says why ...
      fs.writeFileSync(modeFile, 'refuse-model=gpt-6-astra');
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Shed roof', body: 'Fixed the shed roof.', pages: ['shed'] });
      const reason = await waitFor(async () => (await getJson(server.port, '/api/status')).json.reasons.find((x) => /Codex refused the model gpt-6-astra with high reasoning/.test(x)), 20_000, 'the refusal in /status');
      assert.match(reason, /choose another: window > Status > Models/);
      assert.equal((await getJson(server.port, '/api/status')).json.queue.dead, 0, 'a refused model fails no note');
      // ... and tries again as soon as another model is chosen (the pause would last 10 minutes), with no restart.
      r = await postJson(server.port, '/api/models', { role: 'curator', model: 'gpt-6.1-sol', reasoningEffort: 'high' }, UI);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      await waitFor(async () => (await getJson(server.port, '/api/status')).json.queue.pending === 0 && fs.existsSync(path.join(E.wikiDir, 'pages', 'shed.md')), 20_000, 'the note filed with the new model');
      assert.match(lastExec(), /-m gpt-6\.1-sol .*model_reasoning_effort="high"/);
      curator.proc.stdin.end();
      await curator.done;
      curator = null;
      // The CLI: Ask gets its own model; a refusal names the fix; reset goes back to the default.
      let s = cli('set', 'ask', 'gpt-6-luna', '--effort', 'medium');
      assert.equal(s.status, 0, s.stderr);
      assert.match(s.stdout, /Ask: +gpt-6-luna, medium reasoning \(chosen\)/);
      s = cli('set', 'ask', 'gpt-9');
      assert.equal(s.status, 1);
      assert.match(s.stderr, /gpt-9 is not in this ChatGPT account's model list .*--force/);
      s = cli('reset', 'ask');
      assert.equal(s.status, 0, s.stderr);
      assert.match(s.stdout, /Ask: +gpt-6\.1-sol, low reasoning \(default; the curator's model\)/);
      s = cli();
      assert.match(s.stdout, /Curator: gpt-6\.1-sol, high reasoning \(chosen\)/);
      assert.match(s.stdout, /gpt-6-astra +medium high max/);
      // A hand-edited choice that could reach Codex's command line is ignored.
      const file = path.join(E.wikiDir, '.curator', 'settings.json');
      fs.writeFileSync(file, JSON.stringify({ models: { curator: { model: 'x --dangerously-bypass', reasoningEffort: 'low"\nsandbox_mode="danger-full-access' } } }));
      o = await models();
      assert.deepEqual([o.curator.model, o.curator.reasoningEffort], ['gpt-6.1-sol', 'medium']);
      const log = allLogText(E.wikiDir);
      assert.match(log, /· window · Curator model set: gpt-6-astra, high reasoning/);
      assert.match(log, /· cli · Ask model set: gpt-6-luna, medium reasoning/);
      assert.match(log, /· cli · Ask model back to the default/);
    } finally {
      if (curator) {
        curator.proc.stdin.end();
        await curator.done;
      }
      await c.close();
      server.proc.stdin.end();
    }
  });

  test('by default a held change is applied at once, shown for the notification, and can be undone; "Ask me first" holds again', async () => {
    const E = makeEnv();
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    const UI = { 'X-Agent-Wiki': 'ui' };
    try {
      const page = path.join(E.wikiDir, 'pages', 'harbor.md');
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Harbor runs on port 8443', body: 'Staging and production use port 8443.', pages: ['harbor'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      const before = fs.readFileSync(page, 'utf8');
      assert.equal((await getJson(server.port, '/api/settings')).json.approvals, 'auto');

      await ok(c, 'wiki_log', { app: 'codex', source: 'external', title: 'Forum says Harbor moved', body: 'A forum post says Harbor moved to port 9443.', pages: ['harbor'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.match(fs.readFileSync(page, 'utf8'), /Forum says Harbor moved/, 'applied without waiting');
      const log = allLogText(E.wikiDir);
      assert.match(log, /· curator · Change applied automatically: Harbor \[\[harbor\]\]/);
      assert.doesNotMatch(log, /waiting for your OK/);
      const st = (await getJson(server.port, '/api/status')).json;
      assert.equal(st.held, 0);
      assert.equal(st.approvals, 'auto');
      assert.equal(st.autoApplied.count, 1);
      assert.equal(st.autoApplied.times.length, 1, 'the tray announces each new time once');
      assert.equal(st.autoApplied.last.slug, 'harbor');
      const held = (await getJson(server.port, '/api/held')).json;
      assert.deepEqual(held.changes, []);
      assert.equal(held.approvals, 'auto');
      const [a] = held.auto;
      assert.equal(a.status, 'approved');
      assert.match(a.reasons[0], /external content/);
      assert.equal(a.undoable, true);
      assert.ok(a.diff.some(([k, l]) => k === '+' && /Forum says Harbor moved/.test(l)), 'the Inbox can show what changed');

      assert.equal((await postJson(server.port, '/api/held', { batch: a.batch, index: a.index, action: 'undo' })).status, 403, 'needs the window header');
      const u = await postJson(server.port, '/api/held', { batch: a.batch, index: a.index, action: 'undo' }, UI);
      assert.equal(u.status, 200, JSON.stringify(u.json));
      assert.equal(fs.readFileSync(page, 'utf8'), before, 'the page is back as it was');
      const after = (await getJson(server.port, '/api/held')).json.auto[0];
      assert.deepEqual([after.status, after.undoable], ['undone', false]);
      assert.equal((await postJson(server.port, '/api/held', { batch: a.batch, index: a.index, action: 'undo' }, UI)).status, 409, 'undone once');
      assert.match(allLogText(E.wikiDir), /· window · Undid a change: Harbor \[\[harbor\]\]/);

      // "Ask me first": the next one waits; switching back to automatic applies it.
      assert.equal((await postJson(server.port, '/api/settings', { approvals: 'manual' })).status, 403, 'needs the window header');
      assert.equal((await getJson(server.port, '/api/settings')).json.approvals, 'auto', 'a refused request changes nothing');
      assert.equal((await postJson(server.port, '/api/settings', { approvals: 'sometimes' }, UI)).status, 400);
      assert.equal((await postJson(server.port, '/api/settings', { approvals: 'manual' }, UI)).json.approvals, 'manual');
      await ok(c, 'wiki_log', { app: 'codex', source: 'external', title: 'Blog says Harbor uses TLS 1.3', body: 'A blog post says Harbor only accepts TLS 1.3.', pages: ['harbor'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.doesNotMatch(fs.readFileSync(page, 'utf8'), /Blog says Harbor/);
      assert.equal((await getJson(server.port, '/api/status')).json.held, 1);
      const sw = await postJson(server.port, '/api/settings', { approvals: 'auto' }, UI);
      assert.deepEqual(sw.json, { approvals: 'auto', applied: 1 });
      assert.match(fs.readFileSync(page, 'utf8'), /Blog says Harbor uses TLS 1\.3/);
      const st2 = (await getJson(server.port, '/api/status')).json;
      assert.equal(st2.held, 0);
      assert.equal(st2.autoApplied.count, 2);

      // A change left waiting when the setting goes back to the default (an upgrade) goes in when the curator starts.
      await postJson(server.port, '/api/settings', { approvals: 'manual' }, UI);
      await ok(c, 'wiki_log', { app: 'codex', source: 'external', title: 'Wiki says Harbor runs 3 replicas', body: 'A wiki page says Harbor runs 3 replicas.', pages: ['harbor'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.equal((await getJson(server.port, '/api/status')).json.held, 1);
      fs.rmSync(path.join(E.wikiDir, '.curator', 'settings.json'));
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.match(fs.readFileSync(page, 'utf8'), /Wiki says Harbor runs 3 replicas/);
      assert.equal((await getJson(server.port, '/api/status')).json.autoApplied.count, 3);
      assert.match(allLogText(E.wikiDir), /· window · Changes and cleanups now wait for your OK[\s\S]*· window · Changes and cleanups are now applied automatically/);
    } finally {
      await c.close();
      server.proc.stdin.end();
    }
  });

  test('forget (M6): a request from the user waits for an OK, then every copy is redacted', async () => {
    const E = makeEnv();
    const server = await startHttp(E.env);
    const c = await connect('http', E.env, 'claude-code', server.url);
    const UI = { 'X-Agent-Wiki': 'ui' };
    try {
      const phone = '555-0142-7781';
      await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'Dana phone', body: `Dana's old phone is ${phone}.`, pages: ['dana'] });
      assert.equal((await runCurator(E.env).done).code, 0);
      assert.match(fs.readFileSync(path.join(E.wikiDir, 'pages', 'dana.md'), 'utf8'), new RegExp(phone));

      // An app's own request to forget is not enough; the user's is recorded and waits.
      await ok(c, 'wiki_log', { app: 'codex', title: 'FORGET a number', body: phone });
      const userNote = (await ok(c, 'wiki_log', { app: 'claude-code', source: 'user', title: 'FORGET Dana old phone', body: phone })).match(/^Saved note (\S+)/)[1];
      assert.equal((await runCurator(E.env).done).code, 0);
      const st = (await getJson(server.port, '/api/status')).json;
      assert.equal(st.held, 1, JSON.stringify(st.reasons));
      const { forget } = (await getJson(server.port, '/api/held')).json;
      assert.equal(forget.length, 1);
      assert.equal(forget[0].text, phone);
      assert.deepEqual(forget[0].noteIds, [userNote], "the user's request, not the app's");
      assert.ok(forget[0].matches >= 3 && forget[0].files.includes('pages/dana.md'), JSON.stringify(forget[0]));
      assert.ok(fs.readFileSync(path.join(E.wikiDir, 'pages', 'dana.md'), 'utf8').includes(phone), 'nothing changes before the OK');

      const preview = await postJson(server.port, '/api/forget', { text: phone }, UI);
      assert.equal(preview.status, 200);
      assert.ok(preview.json.found.length >= 3 && preview.json.found.some((f) => f.rel === 'pages/dana.md'), JSON.stringify(preview.json));
      assert.ok(preview.json.found.every((f) => !f.sample.includes(phone)), 'previews are masked');
      assert.equal((await postJson(server.port, '/api/forget', { text: phone })).status, 403);
      assert.equal((await postJson(server.port, '/api/forget', { text: phone, apply: true })).status, 403, 'redacting needs the window header');
      assert.equal((await postJson(server.port, '/api/forget', { batch: forget[0].batch, index: forget[0].index, action: 'approve' })).status, 403);
      assert.ok(fs.readFileSync(path.join(E.wikiDir, 'pages', 'dana.md'), 'utf8').includes(phone), 'refused requests change nothing');

      const r = await postJson(server.port, '/api/forget', { batch: forget[0].batch, index: forget[0].index, action: 'approve' }, UI);
      assert.equal(r.status, 200, JSON.stringify(r.json));
      assert.ok(r.json.matches >= 3);
      const everywhere = walk(E.wikiDir).filter((f) => !f.includes(`${path.sep}.locks${path.sep}`)).concat(walk(E.logs));
      assert.deepEqual(everywhere.filter((f) => fs.readFileSync(f, 'utf8').includes(phone)), [], 'no copy left in the wiki or the logs');
      assert.equal((await getJson(server.port, '/api/status')).json.held, 0);
      assert.match(allLogText(E.wikiDir), /Forgot a piece of text: \d+ match\(es\) redacted/);
    } finally {
      await c.close();
      server.proc.stdin.end();
    }
  });
});

describe('request log', () => {
  test('8 processes appending at once: every line intact; secrets redacted; old files swept', async () => {
    const dir = path.join(tmp, 'reqlog');
    const mod = JSON.stringify(new URL('../src/reqlog.mjs', import.meta.url).href);
    const procs = Array.from({ length: 8 }, (_, p) =>
      spawn(
        process.execPath,
        [
          '--input-type=module',
          '-e',
          `import { createRequestLog } from ${mod};
           const l = createRequestLog({ dir: ${JSON.stringify(dir)}, proc: 'p${p}' });
           for (let i = 0; i < 400; i++) l.write({ kind: 'tool', rid: 'p${p}-' + i, args: { body: 'x'.repeat(${p} * 300) } });`,
        ],
        { stdio: 'inherit' },
      ),
    );
    await Promise.all(procs.map((p) => new Promise((r) => p.on('exit', r))));
    const lines = requestLines(dir);
    assert.equal(lines.length, 3200);
    assert.equal(new Set(lines.map((l) => l.rid)).size, 3200);

    const { createRequestLog, sweep, summarizeArgs } = await import('../src/reqlog.mjs');
    const l = createRequestLog({ dir, proc: 'test' });
    l.write({ kind: 'tool', args: summarizeArgs({ title: 'rotate', body: `password: Hunter2Hunter2xyz and ${SECRET}` }) });
    const text = walk(dir).map((f) => fs.readFileSync(f, 'utf8')).join('');
    assert.ok(!text.includes('Hunter2Hunter2xyz') && !text.includes(SECRET));
    fs.writeFileSync(path.join(dir, 'requests-2020-01-01.jsonl'), '{}\n');
    assert.equal(sweep(dir, 30), 1);
    assert.ok(!fs.existsSync(path.join(dir, 'requests-2020-01-01.jsonl')));
  });
});
