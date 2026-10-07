// The memory eval: how well Agent Wiki finds what it knows, and how well its curator files what it is told.
//
//   npm run eval                          search tier + the Ask and curator plumbing with the fake model
//   npm run eval -- --live                the same with the real model (Ask answers judged by the model)
//   npm run eval -- --live --set local    your real wiki (a snapshot) and eval/local/questions.json (uncommitted)
//   options: --tier search,ask,curate     --concurrency 3     --only <id,id>     --out <file.json>
//
// Tiers:
//   search   wiki_search alone, no model: is an evidence file in the top 1/3/8 results, for the question
//            as asked and for the words a good agent would search for? (deterministic: test/memory-eval.mjs)
//   ask      the real Ask pipeline (server + curator --asks-only, as the window uses it) on a copy of the
//            wiki: did it read the evidence, cite it, contain the expected facts, abstain when it should?
//   curate   each note stream through wiki_log and `curator.mjs --once` on a fresh copy: do the pages
//            say what they should afterwards (updates, history, poisoning, duplicates, secrets)?
//
// The categories follow LongMemEval (extraction, multi-session, temporal, knowledge update, abstention).
// Live runs use their own Codex sign-in (AGENT_WIKI_EVAL_CODEX_HOME, default <data>-dev/eval-codex-home),
// never the curator's from inside an app package, where AppData writes are redirected.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { appPaths } from '../src/paths.mjs';
import { curatorCmd, rustBin, srv, srvT } from '../test/impl.mjs';

const REPO = fileURLToPath(new URL('..', import.meta.url));
const RUNTIME = path.join(REPO, 'dist', 'runtime');
const FAKE_CODEX = path.join(REPO, 'test', 'fixtures', 'fake-codex.mjs');
const fwd = (p) => String(p).replace(/\\/g, '/');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
/** Per-question progress on stderr: the CLI turns it on; tests leave it off. */
let progress = () => {};

// ---------------------------------------------------------------- data sets

export function loadSet(name = 'synthetic') {
  if (name === 'synthetic') {
    const dir = path.join(REPO, 'eval', 'synthetic');
    return {
      name,
      dir,
      wikiSrc: path.join(dir, 'wiki'),
      questions: JSON.parse(fs.readFileSync(path.join(dir, 'questions.json'), 'utf8')).questions,
      streams: JSON.parse(fs.readFileSync(path.join(dir, 'streams.json'), 'utf8')).streams,
    };
  }
  // The same wiki with questions written to be hard for word matching: other words for the same
  // thing, answers spread over several files, dates, near misses, and notes to route to a page.
  if (name === 'synthetic-hard') {
    const dir = path.join(REPO, 'eval', 'synthetic');
    return { name, dir, wikiSrc: path.join(dir, 'wiki'), questions: JSON.parse(fs.readFileSync(path.join(dir, 'questions-hard.json'), 'utf8')).questions, streams: [] };
  }
  if (name === 'local') {
    const file = path.join(REPO, 'eval', 'local', 'questions.json');
    if (!fs.existsSync(file)) throw new Error(`No ${fwd(file)}: write your own questions there (kept out of git).`);
    const cfg = JSON.parse(fs.readFileSync(installedPaths().configFile, 'utf8'));
    return { name, wikiSrc: path.resolve(cfg.wikiDir), questions: JSON.parse(fs.readFileSync(file, 'utf8')).questions, streams: [] };
  }
  throw new Error(`Unknown set "${name}" (synthetic, synthetic-hard or local)`);
}

/** The installed locations, whatever AGENT_WIKI_* overrides this shell has. */
function installedPaths() {
  return appPaths({ env: Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^AGENT_WIKI_/.test(k))) });
}

/**
 * A working copy of a wiki: pages, log and PROTOCOL.md (no history, curator state or locks), and with
 * `inbox` the notes waiting for the curator (questions may be answered from them; the curate tier
 * leaves them out, so its streams are the only notes filed).
 */
export async function copyWiki(src, dest, { inbox = false } = {}) {
  await fsp.mkdir(dest, { recursive: true });
  for (const part of ['pages', 'log', ...(inbox ? ['inbox'] : [])]) {
    if (fs.existsSync(path.join(src, part))) await fsp.cp(path.join(src, part), path.join(dest, part), { recursive: true });
  }
  if (fs.existsSync(path.join(src, 'PROTOCOL.md'))) await fsp.copyFile(path.join(src, 'PROTOCOL.md'), path.join(dest, 'PROTOCOL.md'));
  return dest;
}

// ---------------------------------------------------------------- model settings

/** {codexPath, codexHome, model, curatorEffort, askEffort, live}. */
async function modelSettings({ live }) {
  if (!live) return { live, codexPath: FAKE_CODEX, codexHome: null, model: 'fake', curatorEffort: 'medium', askEffort: 'low' };
  const ip = installedPaths();
  const installed = JSON.parse(await fsp.readFile(ip.configFile, 'utf8').catch(() => '{}'));
  const codexPath = installed.curator?.codexPath || 'codex';
  const home = process.env.AGENT_WIKI_EVAL_CODEX_HOME || `${ip.dataDir}-dev${path.sep}eval-codex-home`;
  if (!fs.existsSync(path.join(home, 'auth.json')) && !process.env.AGENT_WIKI_EVAL_CODEX_HOME) {
    throw new Error(
      `Live evals use their own Codex sign-in, in ${fwd(home)} (or AGENT_WIKI_EVAL_CODEX_HOME). Sign it in once:\n` +
        `  CODEX_HOME="${home}" "${codexPath}" login`,
    );
  }
  return {
    live,
    codexPath,
    codexHome: home,
    model: installed.curator?.model || 'gpt-6.1-sol',
    curatorEffort: installed.curator?.reasoningEffort || 'medium',
    askEffort: installed.ask?.reasoningEffort || 'low',
  };
}

async function writeHome(home, ms, { concurrency = 2, curator = {} } = {}) {
  await fsp.mkdir(home, { recursive: true });
  const codexHome = ms.codexHome || path.join(home, 'codex-home');
  await fsp.mkdir(codexHome, { recursive: true });
  const config = {
    writeMode: 'curated',
    curator: { model: ms.model, reasoningEffort: ms.curatorEffort, codexPath: fwd(ms.codexPath), codexHome: fwd(codexHome), debounceSeconds: 0, maxWaitSeconds: 0, timeoutSeconds: 600, ...curator },
    ask: { reasoningEffort: ms.askEffort, maxConcurrent: concurrency, timeoutSeconds: 300 },
  };
  await fsp.writeFile(path.join(home, 'config.json'), `${JSON.stringify(config, null, 2)}\n`);
  return config;
}

const childEnv = (wikiDir, home) => ({
  ...Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^AGENT_WIKI_/.test(k))),
  AGENT_WIKI_DIR: wikiDir,
  AGENT_WIKI_HOME: home,
});

// ---------------------------------------------------------------- scoring helpers

// Markdown emphasis and code marks are formatting, not content ("at most `5` retries").
const norm = (s) => String(s ?? '').toLowerCase().replace(/\\/g, '/').replace(/[`*]/g, '').replace(/\s+/g, ' ');
/** Does `text` contain every fact ("a|b" = either)? Returns the missing ones. */
export function missingFacts(text, facts = []) {
  const t = norm(text);
  return facts.filter((f) => !f.split('|').some((alt) => t.includes(norm(alt))));
}

/** A search/read target ("harbor", "2026-09-21", "pages/x.md") as a wiki-relative path. */
export function relOf(target, kind) {
  const t = fwd(String(target ?? '')).replace(/^\[\[|\]\]$/g, '').replace(/#.*$/, '');
  if (/^\d{4}-\d{2}-\d{2}$/.test(t)) return `log/${t.slice(0, 4)}/${t}.md`;
  if (t.startsWith('note:')) return `inbox/${t.slice(5)}.md`; // the eval's notes are all still in the inbox
  if (kind === 'page' || /^[a-z0-9][a-z0-9-]*$/.test(t)) return `pages/${t}.md`;
  return t;
}

const searchRels = (text) => [...String(text).matchAll(/^\d+\. (\S+) - /gm)].map((m) => m[1]);

function rankOf(rels, evidence) {
  const i = rels.findIndex((r) => evidence.includes(r));
  return i < 0 ? null : i + 1;
}

const pct = (n, d) => (d ? Math.round((1000 * n) / d) / 10 : null);

// ---------------------------------------------------------------- tier: search

export async function searchTier(set, { work, ms = null, review = false, embeddings = null }) {
  const wikiDir = await copyWiki(set.wikiSrc, path.join(work, 'search-wiki'), { inbox: true });
  // --review: first let the curator's weekly review pass over the wiki (with approvals automatic it
  // applies its cleanups, aliases included), so search sees the pages as the curator leaves them.
  let reviewed;
  if (review) {
    const home = path.join(work, 'review-home');
    await writeHome(home, ms, { curator: { lint: 'on' } });
    const r = await runCurator(childEnv(wikiDir, home), 3_600_000, ['--lint']);
    if (r.code !== 0) throw new Error(`the review failed (exit ${r.code}): ${r.out.slice(-600)}`);
    reviewed = {};
    for (const p of await fsp.readdir(path.join(wikiDir, 'pages'))) {
      const m = (await fsp.readFile(path.join(wikiDir, 'pages', p), 'utf8')).match(/^aliases: (.*)$/m);
      reviewed[p.replace(/\.md$/, '')] = m ? JSON.parse(m[1]) : [];
    }
  }
  // --embeddings <model>: hybrid search (docs/search-plan.md, S2) through the program itself: the
  // setting on, the wiki's sections embedded (agent-wiki embeddings sync), then the same searches.
  const searchHome = path.join(work, 'search-home');
  let embedded;
  if (embeddings) {
    await fsp.mkdir(searchHome, { recursive: true });
    const e = { enabled: true, model: embeddings.model, ...(embeddings.credential ? { credential: embeddings.credential } : {}), ...(embeddings.weight ? { weight: embeddings.weight } : {}) };
    await fsp.writeFile(path.join(searchHome, 'config.json'), `${JSON.stringify({ search: { embeddings: e } }, null, 2)}\n`);
    const r = await new Promise((resolve) => {
      const child = spawn(rustBin(), ['embeddings', 'sync'], { env: childEnv(wikiDir, searchHome), stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
      let out = '';
      child.stdout.on('data', (d) => (out += d));
      child.stderr.on('data', (d) => (out += d));
      child.on('exit', (code) => resolve({ code, out }));
    });
    if (r.code !== 0) throw new Error(`embeddings sync failed: ${r.out.slice(-400)}`);
    embedded = { ...e, ...JSON.parse(r.out.trim().split('\n').at(-1)) };
  }
  const routing = [];
  const client = new Client({ name: 'agent-wiki-eval', version: '1' });
  await client.connect(new StdioClientTransport({ ...srvT(RUNTIME, '--read-only'), env: childEnv(wikiDir, searchHome), stderr: 'ignore' }));
  const rows = [];
  try {
    for (const q of set.questions.filter((x) => !x.abstain && !x.note)) {
      const row = { id: q.id, category: q.category };
      for (const [mode, query] of [['question', q.question], ['keywords', q.query]]) {
        const r = await client.callTool({ name: 'wiki_search', arguments: { query, limit: 8 } });
        const rels = searchRels(r.content?.[0]?.text || '');
        row[mode] = rankOf(rels, q.evidence);
        if (row[mode] !== 1) row[`${mode}Top`] = rels.slice(0, 3); // what ranked above the evidence, for tuning
      }
      rows.push(row);
    }
    // Routing: the curator searches pages with a new note's title and the start of its body (top 4)
    // to decide which pages it shows the model (curator.rs, context). A hit: the page it belongs on.
    for (const q of set.questions.filter((x) => x.note)) {
      const query = `${q.note.title} ${q.note.body.slice(0, 400)}`;
      const r = await client.callTool({ name: 'wiki_search', arguments: { query, scope: 'pages', limit: 4 } });
      const rels = searchRels(r.content?.[0]?.text || '');
      routing.push({ id: q.id, rank: rankOf(rels, q.evidence), top: rels });
    }
  } finally {
    await client.close();
  }
  const summary = {};
  for (const mode of ['question', 'keywords']) {
    const ranks = rows.map((r) => r[mode]);
    summary[mode] = {
      'recall@1': pct(ranks.filter((k) => k && k <= 1).length, ranks.length),
      'recall@3': pct(ranks.filter((k) => k && k <= 3).length, ranks.length),
      'recall@8': pct(ranks.filter((k) => k && k <= 8).length, ranks.length),
      mrr: Math.round((1000 * ranks.reduce((s, k) => s + (k ? 1 / k : 0), 0)) / ranks.length) / 1000,
    };
    summary.perCategory ??= {};
    for (const c of [...new Set(rows.map((r) => r.category))]) {
      const cr = rows.filter((r) => r.category === c).map((r) => r[mode]);
      (summary.perCategory[c] ??= { n: cr.length })[mode] = { 'recall@1': pct(cr.filter((k) => k === 1).length, cr.length), 'recall@8': pct(cr.filter((k) => k && k <= 8).length, cr.length) };
    }
  }
  if (routing.length) summary.routing = { n: routing.length, 'recall@4': pct(routing.filter((r) => r.rank).length, routing.length), 'recall@1': pct(routing.filter((r) => r.rank === 1).length, routing.length) };
  return { rows, routing, summary, ...(reviewed ? { aliases: reviewed } : {}), ...(embedded ? { embeddings: embedded } : {}) };
}

// ---------------------------------------------------------------- tier: ask

function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.on('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

function request(port, method, p, body) {
  return new Promise((resolve, reject) => {
    const data = body ? JSON.stringify(body) : null;
    const req = http.request(
      { host: '127.0.0.1', port, method, path: p, headers: { 'X-Agent-Wiki': 'ui', ...(data ? { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(data) } : {}) }, timeout: 15_000 },
      (res) => {
        let s = '';
        res.on('data', (d) => (s += d));
        res.on('end', () => {
          try {
            resolve({ status: res.statusCode, json: JSON.parse(s) });
          } catch {
            resolve({ status: res.statusCode, json: null, text: s });
          }
        });
      },
    );
    req.on('error', reject);
    req.on('timeout', () => req.destroy(new Error('timeout')));
    if (data) req.write(data);
    req.end();
  });
}

function startProc(cmd, env, label, logs) {
  const [command, args] = Array.isArray(cmd[1]) ? cmd : [process.execPath, cmd];
  const child = spawn(command, args, { env, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
  const out = [];
  child.stdout.on('data', (d) => out.push(String(d)));
  child.stderr.on('data', (d) => out.push(String(d)));
  logs.push({ label, out });
  return child;
}

async function stopProc(child) {
  if (!child || child.exitCode !== null) return;
  child.stdin.end();
  const done = new Promise((r) => child.on('exit', r));
  if (!(await Promise.race([done.then(() => true), sleep(15_000).then(() => false)]))) child.kill();
}

/** Asks one question through the HTTP API and waits for the result. */
async function askOne(port, question, timeoutMs) {
  const t0 = Date.now();
  let posted;
  for (let i = 0; i < 30; i++) {
    posted = await request(port, 'POST', '/api/ask', { question });
    if (posted.status !== 503) break; // the worker's first heartbeat may not be there yet
    await sleep(1000);
  }
  if (posted.status < 200 || posted.status > 299 || !posted.json?.id) return { error: `POST /api/ask: ${posted.status} ${JSON.stringify(posted.json || posted.text)}` };
  const id = posted.json.id;
  for (;;) {
    const r = await request(port, 'GET', `/api/ask?id=${encodeURIComponent(id)}&after=0`);
    const a = r.json;
    if (a && ['done', 'error', 'cancelled'].includes(a.status)) return { id, ...a, seconds: Math.round((Date.now() - t0) / 100) / 10 };
    if (Date.now() - t0 > timeoutMs) return { id, error: 'timed out waiting for the answer' };
    await sleep(1000);
  }
}

const JUDGE_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['pass', 'reason'],
  properties: { pass: { type: 'boolean' }, reason: { type: 'string' } },
};

function judgePrompt(q, a) {
  const expected = q.abstain
    ? 'The wiki does not contain this information (or the question rests on a false premise). A correct reply says the wiki does not say, and does not invent an answer.'
    : `A correct reply states: ${q.facts.map((f) => f.split('|')[0]).join('; ')}. These are fragments of the answer, not exact wording: a reply that contains them in a fuller form (a whole path with \\ or / separators, an environment variable such as %LOCALAPPDATA% or an expanded path, another date format) is correct. It may add detail; it must not contradict them or present an outdated value as current.`;
  return `You grade one answer from a personal-wiki assistant. Reply with JSON: pass (true or false) and a one-line reason.

Question: ${q.question}

Expected: ${expected}

The assistant said the wiki ${a.found ? 'answers' : 'does NOT answer'} the question. Its reply:
<reply>
${a.answer || '(no answer)'}
</reply>

Pass only if the reply is correct as described. Be strict about wrong facts; ignore style.`;
}

export async function askTier(set, { work, ms, concurrency, only }) {
  const wikiDir = await copyWiki(set.wikiSrc, path.join(work, 'ask-wiki'), { inbox: true });
  const home = path.join(work, 'ask-home');
  await writeHome(home, ms, { concurrency });
  const port = await freePort();
  const env = childEnv(wikiDir, home);
  if (!ms.live) env.FAKE_CODEX_MODE = 'normal';
  const logs = [];
  const server = startProc(srv(RUNTIME, '--http', '--port', String(port), '--parent-stdin'), env, 'server', logs);
  const worker = startProc(curatorCmd(RUNTIME, '--asks-only', '--parent-stdin'), env, 'asks', logs);
  const questions = set.questions.filter((q) => !q.note && (!only || only.includes(q.id)));
  const rows = [];
  let judge = null;
  if (ms.live) {
    const { runModel, curatorConfig } = await import(pathToFileURL(path.join(RUNTIME, 'curator.mjs')).href);
    const cfg = curatorConfig({ curator: { codexPath: ms.codexPath, codexHome: ms.codexHome, model: ms.model, reasoningEffort: 'low', timeoutSeconds: 180 } });
    judge = async (q, a) => {
      const runDir = path.join(work, 'judge', q.id);
      await fsp.mkdir(runDir, { recursive: true });
      try {
        const r = await runModel(cfg, judgePrompt(q, a), { runDir, schema: JUDGE_SCHEMA, schemaName: 'grade' });
        return r.output;
      } catch (e) {
        return { pass: null, reason: `judge failed: ${e.message}` };
      }
    };
  }
  try {
    for (let i = 0; i < 60 && !(await request(port, 'GET', '/health').catch(() => null))?.json?.ok; i++) await sleep(500);
    const queue = [...questions];
    await Promise.all(
      Array.from({ length: Math.max(1, concurrency) }, async () => {
        for (let q = queue.shift(); q; q = queue.shift()) {
          const a = await askOne(port, q.question, ms.live ? 400_000 : 60_000);
          const steps = (a.events || []).filter((e) => e.type === 'tool' && e.status === 'done');
          const reads = steps.filter((s) => s.tool === 'wiki_read').map((s) => relOf(s.target, s.kind));
          const searched = steps.filter((s) => s.tool === 'wiki_search').flatMap((s) => (s.hits || []).map((h) => relOf(h.target, h.kind)));
          const result = a.result || {};
          const cited = (result.sources || []).map((s) => relOf(s.target, s.kind));
          const ev = q.evidence || [];
          const row = {
            id: q.id,
            category: q.category,
            status: a.error ? 'harness-error' : a.status,
            error: a.error || result.error,
            found: result.found,
            evidenceSearched: ev.length ? ev.some((e) => searched.includes(e)) : null,
            evidenceRead: ev.length ? ev.some((e) => reads.includes(e)) : null,
            cited: ev.length ? ev.some((e) => cited.includes(e)) : null,
            missing: q.abstain ? [] : missingFacts(result.answer, q.facts),
            searches: result.searches,
            reads: result.reads,
            seconds: a.seconds,
          };
          row.factsOk = q.abstain ? result.found === false : result.status === 'done' && row.missing.length === 0;
          if (judge && result.status === 'done') {
            const g = await judge(q, result);
            row.judged = g.pass;
            row.judgeReason = g.reason;
          }
          row.answer = String(result.answer || '').slice(0, 600);
          rows.push(row);
          progress(`  ask ${q.id}: ${row.status}${row.judged === undefined ? '' : row.judged ? ' pass' : ' FAIL'}${row.factsOk ? '' : ' (facts missing)'}\n`);
        }
      }),
    );
  } finally {
    await stopProc(worker);
    await stopProc(server);
  }
  rows.sort((a, b) => a.id.localeCompare(b.id));
  return { rows, summary: summarizeAsk(rows), logs: ms.live ? undefined : logs.map((l) => ({ label: l.label, tail: l.out.join('').slice(-2000) })) };
}

export function summarizeAsk(rows) {
  const by = (pred) => rows.filter(pred);
  const rate = (list, key) => pct(list.filter((r) => r[key] === true).length, list.filter((r) => r[key] !== null && r[key] !== undefined).length);
  const cats = [...new Set(rows.map((r) => r.category))];
  const perCat = Object.fromEntries(
    cats.map((c) => {
      const l = by((r) => r.category === c);
      return [c, { n: l.length, facts: rate(l, 'factsOk'), judged: rate(l, 'judged'), evidenceRead: rate(l, 'evidenceRead') }];
    }),
  );
  const done = by((r) => r.status === 'done');
  const avg = (k) => (done.length ? Math.round((10 * done.reduce((s, r) => s + (Number(r[k]) || 0), 0)) / done.length) / 10 : null);
  return {
    n: rows.length,
    answered: done.length,
    facts: rate(rows, 'factsOk'),
    judged: rate(rows, 'judged'),
    evidenceSearched: rate(rows, 'evidenceSearched'),
    evidenceRead: rate(rows, 'evidenceRead'),
    cited: rate(rows, 'cited'),
    avgSearches: avg('searches'),
    avgReads: avg('reads'),
    avgSeconds: avg('seconds'),
    perCategory: perCat,
  };
}

// ---------------------------------------------------------------- tier: curate

const pageHashes = async (wikiDir) => {
  const out = {};
  for (const f of await fsp.readdir(path.join(wikiDir, 'pages')).catch(() => [])) {
    if (f.endsWith('.md')) out[f.slice(0, -3)] = crypto.createHash('sha256').update(await fsp.readFile(path.join(wikiDir, 'pages', f))).digest('hex');
  }
  return out;
};

/** The text of one `## heading` section of a page body (to the next ## heading). */
export function sectionText(text, heading) {
  const lines = String(text).split(/\r?\n/);
  const i = lines.findIndex((l) => /^##\s+/.test(l) && l.replace(/^##\s+/, '').trim().toLowerCase() === heading.toLowerCase());
  if (i < 0) return '';
  const j = lines.findIndex((l, k) => k > i && /^##\s+/.test(l));
  return lines.slice(i + 1, j < 0 ? undefined : j).join('\n');
}

/** The page changes the trust gate is holding for an OK: [{slug, text, reasons}]. */
async function heldChanges(wikiDir) {
  const dir = path.join(wikiDir, '.curator', 'held');
  const files = (await fsp.readdir(dir).catch(() => [])).filter((n) => n.endsWith('.json'));
  const out = [];
  for (const n of files) {
    const h = JSON.parse(await fsp.readFile(path.join(dir, n), 'utf8'));
    for (const c of h.changes) if (c.status === 'pending') out.push({ slug: c.write.slug, text: c.write.text, reasons: c.reasons });
  }
  return out;
}

export async function runChecks(wikiDir, checks, { before, after, rejected }) {
  const pageText = async (slug) => (await fsp.readFile(path.join(wikiDir, 'pages', `${slug}.md`), 'utf8').catch(() => null));
  const all = async () => (await Promise.all(Object.keys(after).map(pageText))).join('\n\n');
  const out = [];
  for (const c of checks) {
    let pass;
    let detail = '';
    if (c.rejected) pass = rejected;
    else if (c.held) {
      // A change to this page waits for an OK, optionally with (or without) some text.
      const h = (await heldChanges(wikiDir)).filter((x) => x.slug === c.held);
      const text = h.map((x) => x.text).join('\n');
      pass = h.length > 0 && (!c.contains || c.contains.split('|').some((alt) => norm(text).includes(norm(alt)))) && (!c.notContains || !norm(text).includes(norm(c.notContains)));
      detail = h.length ? h.map((x) => x.reasons.join('; ')).join(' / ') : 'nothing held';
    } else if (c.notHeld) {
      const h = await heldChanges(wikiDir);
      pass = h.length === 0;
      detail = h.map((x) => `${x.slug}: ${x.reasons.join('; ')}`).join(' / ');
    }
    else if (c.pageUnchanged) pass = before[c.pageUnchanged] === after[c.pageUnchanged];
    else if (c.newPageLinkedFrom) {
      // A page this run created, linked from the given page (a split instead of growing it).
      const created = Object.keys(after).filter((s) => !(s in before));
      const from = (await pageText(c.newPageLinkedFrom)) || '';
      pass = created.some((s) => from.includes(`[[${s}]]`) || from.includes(`[[${s}|`));
      detail = `created: ${created.join(', ') || 'none'}`;
    } else if (c.maxChars) {
      const text = await pageText(c.page);
      pass = text !== null && text.length <= c.maxChars;
      detail = text === null ? `no page ${c.page}` : `${text.length} chars`;
    } else if (c.noPageChanged) pass = JSON.stringify(before) === JSON.stringify(after);
    else {
      let text = c.anyPage ? await all() : await pageText(c.page);
      if (text === null) {
        pass = false;
        detail = `no page ${c.page}`;
      } else {
        if (c.section) text = sectionText(text, c.section);
        if (c.contains) pass = c.contains.split('|').some((alt) => norm(text).includes(norm(alt)));
        else if (c.notContains) pass = !norm(text).includes(norm(c.notContains));
      }
    }
    out.push({ ...c, pass: Boolean(pass), ...(detail ? { detail } : {}) });
  }
  return out;
}

async function runCurator(env, timeoutMs, args = ['--once']) {
  return new Promise((resolve) => {
    const child = spawn(...curatorCmd(RUNTIME, ...args), { env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
    let out = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (out += d));
    const timer = setTimeout(() => child.kill(), timeoutMs);
    child.on('exit', (code) => {
      clearTimeout(timer);
      resolve({ code, out });
    });
  });
}

async function dispositions(wikiDir) {
  const out = [];
  const root = path.join(wikiDir, '.curator', 'audit');
  for (const y of await fsp.readdir(root).catch(() => [])) {
    for (const m of await fsp.readdir(path.join(root, y)).catch(() => [])) {
      for (const f of await fsp.readdir(path.join(root, y, m)).catch(() => [])) {
        const a = JSON.parse(await fsp.readFile(path.join(root, y, m, f), 'utf8'));
        out.push({ title: a.note?.title, disposition: a.disposition, reason: a.reason });
      }
    }
  }
  return out;
}

export async function curateTier(set, { work, ms, concurrency, only }) {
  const streams = set.streams.filter((s) => !only || only.includes(s.id));
  const rows = [];
  const queue = [...streams];
  await Promise.all(
    Array.from({ length: Math.max(1, concurrency) }, async () => {
      for (let s = queue.shift(); s; s = queue.shift()) {
        const dir = path.join(work, 'curate', s.id);
        const wikiDir = await copyWiki(set.wikiSrc, path.join(dir, 'wiki'));
        // Pages only this stream needs (kept out of the shared wiki so the search tier does not see them).
        for (const p of s.setup || []) await fsp.copyFile(path.join(set.dir, p), path.join(wikiDir, 'pages', path.basename(p)));
        // "Ask me first", so the checks see what the trust gate holds (the default applies it at once).
        await fsp.mkdir(path.join(wikiDir, '.curator'), { recursive: true });
        await fsp.writeFile(path.join(wikiDir, '.curator', 'settings.json'), '{"approvals": "manual"}\n');
        const home = path.join(dir, 'home');
        await writeHome(home, ms, { curator: s.curator });
        const env = childEnv(wikiDir, home);
        if (!ms.live) env.FAKE_CODEX_MODE = 'normal';
        // Start the server once so the index exists before anything is filed.
        const client = new Client({ name: 'agent-wiki-eval', version: '1' });
        await client.connect(new StdioClientTransport({ ...srvT(RUNTIME), env, stderr: 'ignore' }));
        const before = await pageHashes(wikiDir);
        let rejected = false;
        try {
          for (const n of s.notes) {
            const r = await client.callTool({ name: 'wiki_log', arguments: { app: n.app || 'eval', title: n.title, body: n.body, tags: n.tags || [], pages: n.pages || [], ...(n.source ? { source: n.source } : {}) } });
            if (r.isError) rejected = true;
          }
        } finally {
          await client.close();
        }
        const t0 = Date.now();
        let run = s.notes.length ? await runCurator(env, ms.live ? 900_000 : 120_000) : { code: 0, out: '' };
        // A stream can ask for page reviews (the lint) after its notes are filed. Only the Rust curator has them.
        if (s.lint?.length && run.code === 0 && process.env.AGENT_WIKI_IMPL === 'rust') run = await runCurator(env, ms.live ? 900_000 : 120_000, ['--lint', ...s.lint]);
        const after = await pageHashes(wikiDir);
        const checks = await runChecks(wikiDir, s.checks, { before, after, rejected });
        rows.push({
          id: s.id,
          category: s.category,
          curatorExit: run.code,
          seconds: Math.round((Date.now() - t0) / 100) / 10,
          pass: checks.every((c) => c.pass),
          checks: checks.map((c) => ({ why: c.why, pass: c.pass, ...(c.detail ? { detail: c.detail } : {}) })),
          dispositions: await dispositions(wikiDir),
          ...(run.code === 0 ? {} : { curatorTail: run.out.slice(-1500) }),
        });
        progress(`  curate ${s.id}: ${checks.every((c) => c.pass) ? 'pass' : 'FAIL'} (${checks.filter((c) => c.pass).length}/${checks.length})\n`);
      }
    }),
  );
  rows.sort((a, b) => a.id.localeCompare(b.id));
  const allChecks = rows.flatMap((r) => r.checks);
  const cats = [...new Set(rows.map((r) => r.category))];
  return {
    rows,
    summary: {
      streams: rows.length,
      streamsPassed: rows.filter((r) => r.pass).length,
      checks: allChecks.length,
      checksPassed: allChecks.filter((c) => c.pass).length,
      perCategory: Object.fromEntries(cats.map((c) => [c, `${rows.filter((r) => r.category === c && r.pass).length}/${rows.filter((r) => r.category === c).length}`])),
    },
  };
}

// ---------------------------------------------------------------- CLI

export async function runEval({ set = 'synthetic', tiers = ['search', 'ask', 'curate'], live = false, concurrency = 3, only = null, keep = false, review = false, embeddings = null } = {}) {
  const data = loadSet(set);
  const ms = await modelSettings({ live });
  const work = await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-eval-'));
  const report = { at: new Date().toISOString(), set, live, model: ms.model, curatorEffort: ms.curatorEffort, askEffort: ms.askEffort, tiers: {} };
  try {
    if (tiers.includes('search')) report.tiers.search = await searchTier(data, { work, ms, review, embeddings });
    if (tiers.includes('ask')) report.tiers.ask = await askTier(data, { work, ms, concurrency, only });
    if (tiers.includes('curate') && data.streams.length) report.tiers.curate = await curateTier(data, { work, ms, concurrency, only });
  } finally {
    if (!keep) await fsp.rm(work, { recursive: true, force: true, maxRetries: 5 }).catch(() => {});
  }
  return report;
}

function printReport(r) {
  const line = (s) => process.stdout.write(`${s}\n`);
  line(`\nAgent Wiki memory eval: set ${r.set}, ${r.live ? `live (${r.model}; Ask ${r.askEffort}, curator ${r.curatorEffort})` : 'fake model'}`);
  const s = r.tiers.search?.summary;
  if (s) {
    line('\nsearch (no model)              recall@1  recall@3  recall@8   MRR');
    for (const mode of ['question', 'keywords']) line(`  ${mode.padEnd(28)} ${String(s[mode]['recall@1']).padStart(7)}%  ${String(s[mode]['recall@3']).padStart(7)}%  ${String(s[mode]['recall@8']).padStart(7)}%  ${s[mode].mrr}`);
    for (const [c, v] of Object.entries(s.perCategory || {})) line(`    ${c.padEnd(16)} n=${String(v.n).padEnd(3)} as asked @1 ${String(v.question['recall@1']).padStart(5)}%  @8 ${String(v.question['recall@8']).padStart(5)}%   good words @1 ${String(v.keywords['recall@1']).padStart(5)}%`);
    if (s.routing) line(`  routing (note -> its page, pages top 4): ${s.routing['recall@4']}% in the top 4, ${s.routing['recall@1']}% first, n=${s.routing.n}`);
  }
  const a = r.tiers.ask?.summary;
  if (a) {
    line(`\nask (${a.answered}/${a.n} answered)    facts ${a.facts}%   judged ${a.judged ?? '-'}%   evidence read ${a.evidenceRead}%   cited ${a.cited}%`);
    line(`  per answer: ${a.avgSearches} searches, ${a.avgReads} reads, ${a.avgSeconds} s`);
    for (const [c, v] of Object.entries(a.perCategory)) line(`  ${c.padEnd(18)} n=${String(v.n).padEnd(3)} facts ${String(v.facts).padStart(5)}%   judged ${String(v.judged ?? '-').padStart(5)}%   evidence read ${v.evidenceRead ?? '-'}%`);
  }
  const c = r.tiers.curate?.summary;
  if (c) {
    line(`\ncurate: ${c.streamsPassed}/${c.streams} streams, ${c.checksPassed}/${c.checks} checks`);
    for (const row of r.tiers.curate.rows) line(`  ${row.pass ? 'pass' : 'FAIL'}  ${row.id.padEnd(20)} ${row.checks.map((x) => (x.pass ? '+' : '-')).join('')}  ${row.checks.filter((x) => !x.pass).map((x) => x.why).join('; ')}`);
  }
}

const args = process.argv.slice(2);
const opt = (n) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : undefined;
};
if (process.argv[1] && fs.realpathSync(path.resolve(process.argv[1])) === fs.realpathSync(fileURLToPath(import.meta.url))) {
  progress = (s) => process.stderr.write(s);
  const report = await runEval({
    set: opt('--set') || 'synthetic',
    tiers: (opt('--tier') || 'search,ask,curate').split(','),
    live: args.includes('--live'),
    concurrency: Number(opt('--concurrency') || 3),
    only: opt('--only')?.split(',') || null,
    keep: args.includes('--keep'),
    review: args.includes('--review'),
    embeddings: opt('--embeddings') ? { model: opt('--embeddings'), credential: opt('--credential'), weight: opt('--weight') ? Number(opt('--weight')) : undefined } : null,
  }).catch((e) => {
    console.error(`eval failed: ${e.message}`);
    process.exit(1);
  });
  printReport(report);
  const out = opt('--out') || path.join(REPO, '.tmp', 'eval', `${report.at.replace(/[:.]/g, '-')}-${report.set}-${report.live ? 'live' : 'fake'}.json`);
  await fsp.mkdir(path.dirname(out), { recursive: true });
  await fsp.writeFile(out, `${JSON.stringify(report, null, 2)}\n`);
  process.stdout.write(`\nreport: ${fwd(out)}\n`);
}
