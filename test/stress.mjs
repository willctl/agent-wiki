// Opt-in stress test: AGENT_WIKI_STRESS=1 npm run test:stress
// (AGENT_WIKI_RUNTIME=<dir> runs it against another build, e.g. the installed %LOCALAPPDATA%\AgentWiki\runtime.)
//
// A throwaway wiki under heavy concurrent use, the way several apps use the real one at once: two
// HTTP servers (the service and a second one) and eight stdio servers (one per app session), sixteen
// clients firing writes, page hand-overs, reads and searches in parallel bursts, the same
// idempotency keys from four clients at once, a person editing a page meanwhile, one stdio server
// killed mid-run, and the curator (with the fake model) filing everything. Then it checks that
// every acknowledged note was filed exactly once, duplicates collapsed, the human edit survived,
// no lock or temp file was left, and every request-log line is whole.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { curatorCmd, srv, srvT } from './impl.mjs';

const repo = fileURLToPath(new URL('..', import.meta.url));
const runtime = process.env.AGENT_WIKI_RUNTIME || path.join(repo, 'dist', 'runtime');
const fakeCodex = path.join(repo, 'test', 'fixtures', 'fake-codex.mjs');
const skip = !process.env.AGENT_WIKI_STRESS && 'set AGENT_WIKI_STRESS=1 to run';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const HTTP_SERVERS = 2;
const HTTP_CLIENTS = 8;
const STDIO_CLIENTS = 8;
const NOTES_PER_CLIENT = 25;
const UPSERTS_PER_CLIENT = 5;
const READS_PER_CLIENT = 12;
const SHARED_KEYS = 10; // each sent by 4 clients at once
const SLUGS = ['alpha', 'beta', 'gamma', 'delta'];

function walk(dir) {
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => (e.isDirectory() ? walk(path.join(dir, e.name)) : [path.join(dir, e.name)]));
}

function startHttp(env) {
  return new Promise((resolve, reject) => {
    const proc = spawn(...srv(runtime, '--http', '--port', '0', '--parent-stdin'), { env, stdio: ['pipe', 'pipe', 'pipe'] });
    let err = '';
    const timer = setTimeout(() => reject(new Error(`server did not start:\n${err}`)), 15_000);
    proc.stderr.setEncoding('utf8');
    proc.stderr.on('data', (d) => {
      err += d;
      const m = err.match(/listening on (http:\/\/127\.0\.0\.1:(\d+)\/mcp)/);
      if (m) {
        clearTimeout(timer);
        resolve({ proc, url: m[1] });
      }
    });
    proc.on('exit', (code) => reject(new Error(`server exited ${code}:\n${err}`)));
  });
}

test('stress: 16 clients on 10 server processes and the curator, one server killed mid-run', { skip, timeout: 600_000 }, async () => {
  const tmp = fs.realpathSync.native(await fsp.mkdtemp(path.join(os.tmpdir(), 'agent-wiki-stress-')));
  if (!process.env.AGENT_WIKI_KEEP_TEST_TMP) after(() => fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {}));
  const wikiDir = path.join(tmp, 'wiki');
  const home = path.join(tmp, 'home');
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(
    path.join(home, 'config.json'),
    JSON.stringify({ writeMode: 'curated', curator: { codexPath: fakeCodex, lint: 'off', debounceSeconds: 0.3, maxWaitSeconds: 2, pollSeconds: 1, batchMax: 12, maxAttempts: 3, timeoutSeconds: 60 } }),
  );
  const env = { ...process.env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: home };
  delete env.AGENT_WIKI_WRITE_MODE;
  delete env.AGENT_WIKI_FAULT;

  const servers = await Promise.all(Array.from({ length: HTTP_SERVERS }, () => startHttp(env)));
  const clients = [];
  for (let i = 0; i < HTTP_CLIENTS; i++) {
    const c = new Client({ name: `stress-http-${i}`, version: '1.0.0' });
    await c.connect(new StreamableHTTPClientTransport(new URL(servers[i % HTTP_SERVERS].url)));
    clients.push({ c, name: `http-${i}` });
  }
  for (let i = 0; i < STDIO_CLIENTS; i++) {
    const t = new StdioClientTransport({ ...srvT(runtime), env, stderr: 'pipe' });
    const c = new Client({ name: `stress-stdio-${i}`, version: '1.0.0' });
    await c.connect(t);
    clients.push({ c, t, name: `stdio-${i}` });
  }
  const curator = spawn(...curatorCmd(runtime, '--parent-stdin'), { env, stdio: ['pipe', 'pipe', 'pipe'] });
  let curatorErr = '';
  curator.stderr.setEncoding('utf8');
  curator.stderr.on('data', (d) => (curatorErr += d));
  const curatorDone = new Promise((r) => curator.on('exit', (code) => r(code)));

  // A person editing a page in an editor (atomic save, only if unchanged since loaded).
  let humanEdits = 0;
  let stopHuman = false;
  const human = (async () => {
    const f = path.join(wikiDir, 'pages', 'alpha.md');
    while (!stopHuman) {
      try {
        if (fs.existsSync(f)) {
          const t = fs.readFileSync(f, 'utf8');
          fs.writeFileSync(`${f}.edit`, `${t.trimEnd()}\nhuman line ${humanEdits}\n`);
          if (fs.readFileSync(f, 'utf8') === t) {
            fs.renameSync(`${f}.edit`, f);
            humanEdits++;
          } else fs.rmSync(`${f}.edit`);
        }
      } catch {
        fs.rmSync(`${f}.edit`, { force: true }); // the file was being replaced: try again
      }
      await sleep(300);
    }
  })();

  const victim = clients[HTTP_CLIENTS + 3]; // a stdio session that dies mid-run, as if its app crashed
  let killed = false;
  const acked = new Map(); // title -> client name, for every note the server confirmed
  const latencies = [];
  const failures = [];
  const keyResults = [];
  let done = 0;
  const total = clients.length * (NOTES_PER_CLIENT + UPSERTS_PER_CLIENT + READS_PER_CLIENT) + SHARED_KEYS * 4;

  const call = async (cl, name, args) => {
    const t0 = performance.now();
    try {
      const r = await cl.c.callTool({ name, arguments: args });
      latencies.push({ name, ms: performance.now() - t0 });
      const text = r.content.map((x) => x.text).join('\n');
      const notYet = name === 'wiki_read' && /^Not found: /.test(text); // the curator has not created that page yet
      if (r.isError && !notYet) failures.push({ client: cl.name, name, text });
      return { ok: !r.isError, text };
    } catch (e) {
      if (!(killed && cl === victim)) failures.push({ client: cl.name, name, text: String(e?.message || e) });
      return { ok: false, text: String(e) };
    } finally {
      if (++done === Math.floor(total * 0.3) && !killed) {
        killed = true;
        process.kill(victim.t.pid, 'SIGKILL');
      }
    }
  };

  const work = clients.map(async (cl, ci) => {
    const ops = [];
    for (let n = 0; n < NOTES_PER_CLIENT; n++) {
      const title = `Stress note ${cl.name} #${String(n).padStart(2, '0')}`;
      ops.push(async () => {
        const r = await call(cl, 'wiki_log', { app: cl.name, title, body: `body of ${title}`, pages: [SLUGS[(ci + n) % SLUGS.length]], tags: ['stress'] });
        if (r.ok && /^Saved note/.test(r.text)) acked.set(title, cl.name);
      });
    }
    for (let u = 0; u < UPSERTS_PER_CLIENT; u++) {
      const title = `Stress page hand-over ${cl.name} #${u}`;
      ops.push(async () => {
        const r = await call(cl, 'wiki_upsert_page', { app: cl.name, slug: SLUGS[u % SLUGS.length], title, content: `${title}: added by ${cl.name}` });
        if (r.ok && /^Saved note/.test(r.text)) acked.set(title, cl.name);
      });
    }
    for (let q = 0; q < READS_PER_CLIENT; q++) {
      const pick = q % 4;
      ops.push(async () => {
        if (pick === 0) await call(cl, 'wiki_start', { app: cl.name, topic: 'stress' });
        else if (pick === 1) await call(cl, 'wiki_search', { query: `stress note ${q}` });
        else if (pick === 2) {
          const r = await call(cl, 'wiki_read', { target: SLUGS[q % SLUGS.length] });
          if (r.ok) assert.ok(!/^\s*$/.test(r.text), 'a page read is never empty');
        } else await call(cl, 'wiki_read', { target: 'index.md' });
      });
    }
    ops.sort(() => Math.random() - 0.5);
    for (let i = 0; i < ops.length; i += 4) {
      await Promise.all(ops.slice(i, i + 4).map((f) => f())); // bursts of 4 in flight per client
      if (killed && cl === victim) return;
    }
  });
  const shared = Array.from({ length: SHARED_KEYS }, (_, k) =>
    Promise.all(
      [0, 5, 9, 13].map(async (ci) => {
        const cl = clients[ci];
        const r = await call(cl, 'wiki_log', { app: 'shared', title: `Shared retry ${k}`, body: 'sent by four apps at once', idempotency_key: `stress-key-${k}`, pages: ['beta'] });
        keyResults.push({ k, ok: r.ok, saved: /^Saved note/.test(r.text), dup: /^Already saved/.test(r.text) });
      }),
    ),
  );

  try {
    const t0 = Date.now();
    await Promise.all([...work, ...shared]);
    const wroteMs = Date.now() - t0;
    assert.ok(killed, 'the victim server was killed mid-run');
    assert.deepEqual(failures, [], 'every call except the killed session succeeded');

    for (let k = 0; k < SHARED_KEYS; k++) {
      const rs = keyResults.filter((r) => r.k === k);
      assert.equal(rs.filter((r) => r.saved).length, 1, `key ${k}: saved once`);
      assert.equal(rs.filter((r) => r.dup).length, 3, `key ${k}: three duplicates`);
    }

    const inbox = () => (fs.existsSync(path.join(wikiDir, 'inbox')) ? fs.readdirSync(path.join(wikiDir, 'inbox')).filter((n) => n.endsWith('.md')) : []);
    const deadline = Date.now() + 300_000;
    while (inbox().length && Date.now() < deadline) await sleep(500);
    assert.deepEqual(inbox(), [], 'the curator drained the inbox');
    stopHuman = true;
    await human;
    curator.stdin.end();
    assert.equal(await curatorDone, 0, curatorErr);

    // Exactly once: every acknowledged note has one audit record, one log entry, and integrated ones one page line.
    const audits = walk(path.join(wikiDir, '.curator', 'audit')).map((f) => JSON.parse(fs.readFileSync(f, 'utf8')));
    const byTitle = new Map();
    for (const a of audits) byTitle.set(a.note.title, (byTitle.get(a.note.title) || 0) + 1);
    const victimInFlight = [...byTitle.keys()].filter((t) => !acked.has(t) && t.includes(victim.name));
    for (const [t] of acked) assert.equal(byTitle.get(t), 1, `${t}: one audit record`);
    for (let k = 0; k < SHARED_KEYS; k++) assert.equal(byTitle.get(`Shared retry ${k}`), 1, `shared key ${k}: filed once`);
    assert.equal(audits.length, acked.size + SHARED_KEYS + victimInFlight.length, 'no unacknowledged notes except the killed session\'s in-flight ones');
    const logText = walk(path.join(wikiDir, 'log')).map((f) => fs.readFileSync(f, 'utf8')).join('\n');
    const pagesText = SLUGS.map((s) => fs.readFileSync(path.join(wikiDir, 'pages', `${s}.md`), 'utf8')).join('\n');
    for (const a of audits) {
      assert.equal(logText.split(`· ${a.note.title}\n`).length - 1, 1, `${a.note.title}: one log entry`);
      if (a.disposition === 'integrated' && a.note.kind === 'log') assert.equal(pagesText.split(`- ${a.note.title}:`).length - 1, 1, `${a.note.title}: in a page once`);
    }
    assert.ok(humanEdits > 0, 'the person edited during the run');
    assert.match(fs.readFileSync(path.join(wikiDir, 'pages', 'alpha.md'), 'utf8'), new RegExp(`human line ${humanEdits - 1}\\n`), 'the last human edit survived');
    assert.deepEqual(fs.readdirSync(path.join(wikiDir, '.locks')).filter((n) => n !== 'alive'), [], 'no lock left behind (the killed server\'s was broken)');
    assert.deepEqual(walk(path.join(wikiDir, 'pages')).filter((f) => !f.endsWith('.md')), [], 'no temp files among the pages');

    // The request log: whole lines only, each under 4 KB, one tool line per call that reached a server.
    const lines = walk(path.join(home, 'logs')).filter((f) => /requests-.*\.jsonl$/.test(f)).flatMap((f) => fs.readFileSync(f, 'utf8').split('\n').filter(Boolean));
    for (const l of lines) assert.ok(Buffer.byteLength(l) < 4096, 'line under 4 KB');
    const entries = lines.map((l) => JSON.parse(l));
    const toolLines = entries.filter((e) => e.kind === 'tool').length;
    assert.ok(toolLines >= latencies.length, `every answered call is logged (${toolLines} lines, ${latencies.length} answered)`);

    const pct = (xs, p) => xs.sort((a, b) => a - b)[Math.min(xs.length - 1, Math.floor(xs.length * p))].toFixed(0);
    const writes = latencies.filter((l) => l.name === 'wiki_log' || l.name === 'wiki_upsert_page').map((l) => l.ms);
    const reads = latencies.filter((l) => !(l.name === 'wiki_log' || l.name === 'wiki_upsert_page')).map((l) => l.ms);
    const batches = entries.filter((e) => e.kind === 'curator' && e.event === 'batch');
    console.log(
      `stress: ${latencies.length} calls in ${wroteMs} ms from ${clients.length} clients on ${HTTP_SERVERS + STDIO_CLIENTS} servers; ` +
        `writes p50 ${pct(writes, 0.5)} ms p95 ${pct(writes, 0.95)} ms; reads p50 ${pct(reads, 0.5)} ms p95 ${pct(reads, 0.95)} ms; ` +
        `${audits.length} notes filed in ${batches.length} batches; ${humanEdits} human edits kept; killed ${victim.name} (${victimInFlight.length} in-flight note(s) also filed)`,
    );
  } finally {
    stopHuman = true;
    await Promise.allSettled(clients.map((cl) => cl.c.close()));
    if (curator.exitCode === null) curator.kill();
    for (const s of servers) s.proc.kill();
  }
});
