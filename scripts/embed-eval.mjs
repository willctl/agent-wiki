// Compares plain search (the program's BM25), an embedding model alone, and the two fused, on the
// eval sets, for one or more OpenRouter embedding models, with what each costs. Two fusions: RRF
// (k = 60; Cormack et al., SIGIR 2009) and a convex combination of normalized scores (TM2C2: BM25 /
// its list's top score, cosine + 1 over its top + 1; Bruch et al., TOIS 2023) at several weights
// for the semantic side. The weight is chosen on the first set (the 77 questions) and reported on
// the others, so the hard set is never used to tune it.
// Inject OPENROUTER_API_KEY into this process using the platform credential manager.
//
//   node scripts/embed-eval.mjs --models openai/text-embedding-3-small,qwen/qwen3-embedding-8b
//   options: --sets synthetic,synthetic-hard   --k 60
//
// The unit of meaning is a section, as in search: a page's intro (with its title, summary and tags)
// and each ## or ### section, each log entry (a day's one-line entries together), and each note. A
// file's semantic score is its best section's cosine similarity. Embeddings are cached in
// .tmp/embed-cache/<model>/, so a model is billed once for each text. Reports, in the eval's format
// (so scripts/eval-compare.mjs can compare them), go to .tmp/eval/embed/.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { copyWiki, loadSet } from './eval.mjs';
import { srvT } from '../test/impl.mjs';

const REPO = path.resolve(fileURLToPath(new URL('..', import.meta.url)));
const args = process.argv.slice(2);
const opt = (n, d) => (args.includes(n) ? args[args.indexOf(n) + 1] : d);
const MODELS = (opt('--models') || 'openai/text-embedding-3-small').split(',');
const SETS = (opt('--sets') || 'synthetic,synthetic-hard').split(',');
const K = Number(opt('--k', 60));
const API = 'https://openrouter.ai/api/v1';
const KEY = process.env.OPENROUTER_API_KEY;
if (!KEY) {
  console.error('OPENROUTER_API_KEY is not set: run this through the credential helper (see the top of this file).');
  process.exit(2);
}
// Child processes (the search server) never see the key.
const childEnv = Object.fromEntries(Object.entries(process.env).filter(([k]) => k !== 'OPENROUTER_API_KEY'));

// Each model's recommended query and passage forms (from its model card).
function forms(model) {
  if (/qwen3-embedding/.test(model)) return { q: (t) => `Instruct: Given a question about a personal wiki, retrieve the passages that answer it\nQuery: ${t}`, d: (t) => t };
  if (/e5-|nemotron/.test(model)) return { q: (t) => `query: ${t}`, d: (t) => `passage: ${t}` };
  if (/bge-(small|base|large)-en/.test(model)) return { q: (t) => `Represent this sentence for searching relevant passages: ${t}`, d: (t) => t };
  return { q: (t) => t, d: (t) => t };
}

// ---------------------------------------------------------------- sections

function frontmatter(text) {
  const m = text.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/);
  if (!m) return { meta: {}, body: text };
  const meta = {};
  for (const line of m[1].split(/\r?\n/)) {
    const kv = line.match(/^([a-z_]+):\s*(.*)$/);
    if (kv) meta[kv[1]] = kv[2].replace(/^"(.*)"$/, '$1');
  }
  return { meta, body: text.slice(m[0].length) };
}

function sections(wikiDir) {
  const units = [];
  const add = (rel, text) => text.trim() && units.push({ rel, text: text.trim().slice(0, 6000) });
  for (const f of fs.readdirSync(path.join(wikiDir, 'pages')).filter((x) => x.endsWith('.md'))) {
    const rel = `pages/${f}`;
    const { meta, body } = frontmatter(fs.readFileSync(path.join(wikiDir, rel), 'utf8'));
    const title = meta.title || f.slice(0, -3);
    const parts = body.split(/^(?=#{2,3} )/m);
    add(rel, `${title}\n${meta.summary || ''}\n${meta.tags ? `tags: ${meta.tags}` : ''}\n${parts[0].replace(/^# .*$/m, '')}`);
    for (const p of parts.slice(1)) add(rel, `${title} > ${p.replace(/^#{2,3} /, '')}`);
  }
  const logRoot = path.join(wikiDir, 'log');
  for (const y of fs.existsSync(logRoot) ? fs.readdirSync(logRoot) : []) {
    for (const f of fs.readdirSync(path.join(logRoot, y)).filter((x) => x.endsWith('.md'))) {
      const rel = `log/${y}/${f}`;
      const date = f.slice(0, -3);
      const text = fs.readFileSync(path.join(wikiDir, rel), 'utf8');
      const entries = text.split(/^(?=## )/m).slice(1);
      for (const e of entries) add(rel, `Log ${date} ${e.replace(/^## /, '')}`);
      const compact = text.split('\n').filter((l) => /^- [0-9]{2}:[0-9]{2} · /.test(l));
      if (compact.length) add(rel, `Log ${date}\n${compact.join('\n')}`);
    }
  }
  const inbox = path.join(wikiDir, 'inbox');
  for (const f of fs.existsSync(inbox) ? fs.readdirSync(inbox).filter((x) => x.endsWith('.md')) : []) {
    const { meta, body } = frontmatter(fs.readFileSync(path.join(inbox, f), 'utf8'));
    add(`inbox/${f}`, `${meta.title || ''}\n${body}`);
  }
  return units;
}

// ---------------------------------------------------------------- embeddings (cached)

const usage = {};
async function embed(model, texts) {
  const dir = path.join(REPO, '.tmp', 'embed-cache', model.replace(/[^a-z0-9.-]+/gi, '_'));
  fs.mkdirSync(dir, { recursive: true });
  const key = (t) => path.join(dir, `${crypto.createHash('sha256').update(t).digest('hex').slice(0, 32)}.json`);
  const out = new Array(texts.length);
  const todo = [];
  texts.forEach((t, i) => (fs.existsSync(key(t)) ? (out[i] = JSON.parse(fs.readFileSync(key(t), 'utf8'))) : todo.push(i)));
  for (let s = 0; s < todo.length; s += 32) {
    const batch = todo.slice(s, s + 32);
    let r;
    for (let attempt = 1; ; attempt++) {
      r = await fetch(`${API}/embeddings`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${KEY}`, 'Content-Type': 'application/json', 'X-Title': 'Agent Wiki search eval' },
        body: JSON.stringify({ model, input: batch.map((i) => texts[i]) }),
        signal: AbortSignal.timeout(120_000),
      });
      if (r.ok || attempt >= 3 || ![429, 500, 502, 503].includes(r.status)) break;
      await new Promise((res) => setTimeout(res, 2000 * attempt));
    }
    const body = await r.json().catch(() => null);
    if (!r.ok || !Array.isArray(body?.data) || body.data.length !== batch.length) throw new Error(`${model}: HTTP ${r.status} ${JSON.stringify(body?.error || body)?.slice(0, 300)}`);
    const u = (usage[model] ??= { tokens: 0, cost: 0, requests: 0 });
    u.tokens += body.usage?.prompt_tokens || body.usage?.total_tokens || 0;
    u.cost += Number(body.usage?.cost || 0);
    u.requests++;
    body.data.forEach((d, j) => {
      const v = d.embedding;
      const n = Math.hypot(...v) || 1;
      const unit = v.map((x) => x / n);
      out[batch[j]] = unit;
      fs.writeFileSync(key(texts[batch[j]]), JSON.stringify(unit));
    });
  }
  return out;
}

const dot = (a, b) => a.reduce((s, x, i) => s + x * b[i], 0);

// ---------------------------------------------------------------- ranking

/** The program's results as [{rel, score}], best first. */
const searchHits = (text) => [...String(text).matchAll(/^\d+\. (\S+) - .*score ([0-9.e+-]+)\)$/gm)].map((m) => ({ rel: m[1], score: Number(m[2]) }));
const ALPHAS = [0.3, 0.5, 0.6, 0.7, 0.8, 0.9];
const rankOf = (rels, evidence) => {
  const i = rels.findIndex((r) => evidence.includes(r));
  return i < 0 ? null : i + 1;
};

/** Files by their best section's similarity to the query vector, as [{rel, score}]. */
function denseHits(units, vecs, qv, onlyPages) {
  const best = new Map();
  units.forEach((u, i) => {
    if (onlyPages && !u.rel.startsWith('pages/')) return;
    const s = dot(qv, vecs[i]);
    if (!best.has(u.rel) || s > best.get(u.rel)) best.set(u.rel, s);
  });
  return [...best.entries()].sort((a, b) => b[1] - a[1]).map(([rel, score]) => ({ rel, score }));
}

/** Convex combination of normalized scores: alpha * semantic + (1 - alpha) * lexical. BM25 is divided by
 * its top score (its minimum is 0). The cosine is normalized by its theoretical minimum -1 ("tm", TM2C2)
 * or min-max over the files ("mm"), which keeps its spread. */
function convex(lex, dense, alpha, norm = "tm") {
  const lm = lex[0]?.score || 1;
  const hi = dense[0]?.score ?? 0;
  const lo = norm === "mm" ? (dense.at(-1)?.score ?? 0) : -1;
  const score = new Map(dense.map((d) => [d.rel, (alpha * (d.score - lo)) / (hi - lo || 1)]));
  for (const l of lex) score.set(l.rel, (score.get(l.rel) || 0) + ((1 - alpha) * l.score) / lm);
  return [...score.keys()].sort((a, b) => score.get(b) - score.get(a));
}

/** Reciprocal rank fusion of ranked lists (Cormack et al., SIGIR 2009); ties keep the first list's order. */
function rrf(lists) {
  const score = new Map();
  for (const list of lists) list.forEach((rel, i) => score.set(rel, (score.get(rel) || 0) + 1 / (K + i + 1)));
  const first = new Map(lists[0].map((rel, i) => [rel, i]));
  return [...score.keys()].sort((a, b) => score.get(b) - score.get(a) || (first.get(a) ?? 1e9) - (first.get(b) ?? 1e9));
}

const pct = (n, d) => (d ? Math.round((1000 * n) / d) / 10 : null);

function summarize(rows, routing) {
  const summary = {};
  for (const mode of ['question', 'keywords']) {
    const ranks = rows.map((r) => r[mode]);
    summary[mode] = { 'recall@1': pct(ranks.filter((k) => k === 1).length, ranks.length), 'recall@8': pct(ranks.filter((k) => k && k <= 8).length, ranks.length) };
    summary.perCategory ??= {};
    for (const c of [...new Set(rows.map((r) => r.category))]) {
      const cr = rows.filter((r) => r.category === c).map((r) => r[mode]);
      (summary.perCategory[c] ??= { n: cr.length })[mode] = { 'recall@1': pct(cr.filter((k) => k === 1).length, cr.length), 'recall@8': pct(cr.filter((k) => k && k <= 8).length, cr.length) };
    }
  }
  if (routing.length) summary.routing = { n: routing.length, 'recall@4': pct(routing.filter((r) => r.rank).length, routing.length) };
  return summary;
}

// ---------------------------------------------------------------- run

const prices = Object.fromEntries(((await (await fetch(`${API}/embeddings/models`)).json()).data || []).map((m) => [m.id, Number(m.pricing?.prompt || 0) * 1e6]));
const outDir = path.join(REPO, '.tmp', 'eval', 'embed');
fs.mkdirSync(outDir, { recursive: true });
const table = [];

for (const setName of SETS) {
  const set = loadSet(setName);
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'aw-embed-'));
  const wikiDir = await copyWiki(set.wikiSrc, path.join(work, 'wiki'), { inbox: true });
  const units = sections(wikiDir);
  const questions = set.questions.filter((q) => !q.abstain);
  // BM25: the program's own search, deep enough to fuse (50 is wiki_search's maximum).
  const client = new Client({ name: 'agent-wiki-embed-eval', version: '1' });
  await client.connect(new StdioClientTransport({ ...srvT(path.join(REPO, 'dist', 'runtime'), '--read-only'), env: { ...childEnv, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: path.join(work, 'home') }, stderr: 'ignore' }));
  const lexical = {};
  try {
    for (const q of questions) {
      if (q.note) {
        const r = await client.callTool({ name: 'wiki_search', arguments: { query: `${q.note.title} ${q.note.body.slice(0, 400)}`, scope: 'pages', limit: 50 } });
        lexical[q.id] = { route: searchHits(r.content?.[0]?.text) };
      } else {
        lexical[q.id] = {};
        for (const [mode, text] of [['question', q.question], ['keywords', q.query]]) {
          const r = await client.callTool({ name: 'wiki_search', arguments: { query: text, limit: 50 } });
          lexical[q.id][mode] = searchHits(r.content?.[0]?.text);
        }
      }
    }
  } finally {
    await client.close();
  }
  const write = (model, method, rows, routing) => {
    const report = { set: setName, model, method, tiers: { search: { rows, routing, summary: summarize(rows, routing) } } };
    fs.writeFileSync(path.join(outDir, `${setName}__${model.replace(/[^a-z0-9.-]+/gi, '_')}__${method}.json`), `${JSON.stringify(report, null, 1)}\n`);
    return report.tiers.search.summary;
  };
  // The lexical baseline, once per set.
  const rels = (hits) => hits.map((h) => h.rel);
  const bmRows = questions.filter((q) => !q.note).map((q) => ({ id: q.id, category: q.category, question: rankOf(rels(lexical[q.id].question), q.evidence), keywords: rankOf(rels(lexical[q.id].keywords), q.evidence) }));
  const bmRouting = questions.filter((q) => q.note).map((q) => ({ id: q.id, rank: rankOf(rels(lexical[q.id].route).slice(0, 4), q.evidence) }));
  table.push({ set: setName, model: '(none)', method: 'bm25', ...write('bm25', 'bm25', bmRows, bmRouting) });
  for (const model of MODELS) {
    try {
      const f = forms(model);
      const vecs = await embed(model, units.map((u) => f.d(u.text)));
      const qTexts = questions.flatMap((q) => (q.note ? [f.q(`${q.note.title}\n${q.note.body}`)] : [f.q(q.question), f.q(q.query)]));
      const qv = await embed(model, qTexts);
      let j = 0;
      const methods = { dense: (lex, dense) => rels(dense), rrf: (lex, dense) => rrf([rels(lex), rels(dense)]), ...Object.fromEntries(ALPHAS.map((a) => [`cc${a}`, (lex, dense) => convex(lex, dense, a)])), ...Object.fromEntries(ALPHAS.map((a) => [`mm${a}`, (lex, dense) => convex(lex, dense, a, "mm")])) };
      const per = Object.fromEntries(Object.keys(methods).map((m) => [m, { rows: [], routing: [] }]));
      for (const q of questions) {
        if (q.note) {
          const dense = denseHits(units, vecs, qv[j++], true);
          for (const [m, fuse] of Object.entries(methods)) per[m].routing.push({ id: q.id, rank: rankOf(fuse(lexical[q.id].route, dense).slice(0, 4), q.evidence) });
          continue;
        }
        const row = Object.fromEntries(Object.keys(methods).map((m) => [m, { id: q.id, category: q.category }]));
        for (const mode of ['question', 'keywords']) {
          const dense = denseHits(units, vecs, qv[j++], false);
          for (const [m, fuse] of Object.entries(methods)) row[m][mode] = rankOf(fuse(lexical[q.id][mode], dense), q.evidence);
        }
        for (const m of Object.keys(methods)) per[m].rows.push(row[m]);
      }
      for (const method of Object.keys(methods)) table.push({ set: setName, model, method, ...write(model, method, per[method].rows, per[method].routing) });
    } catch (e) {
      console.error(`${setName} ${model}: ${e.message}`);
      table.push({ set: setName, model, method: 'error', error: e.message.slice(0, 120) });
    }
  }
  fs.rmSync(work, { recursive: true, force: true });
}

console.log(`\nunits embedded: sections of the synthetic wiki; RRF k=${K}; @1/@8 = recall at top 1/8 for the question as asked; vocab = the hard set's other-words questions; routing = page in the top 4\n`);
// The convex weight for each model: the best on the first set (as asked, top 1 then top 8).
const tuneSet = SETS[0];
const chosen = {};
for (const model of MODELS) {
  const cands = table.filter((r) => r.set === tuneSet && r.model === model && /^(cc|mm)/.test(r.method));
  cands.sort((a, b) => b.question['recall@1'] - a.question['recall@1'] || b.question['recall@8'] - a.question['recall@8'] || Math.abs(Number(a.method.slice(2)) - 0.6) - Math.abs(Number(b.method.slice(2)) - 0.6));
  if (cands[0]) chosen[model] = cands[0].method;
}
const shown = (r) => r.method === 'bm25' || r.method === 'dense' || r.method === 'rrf' || r.error || chosen[r.model] === r.method;
console.log(`convex weight chosen on "${tuneSet}": ${Object.entries(chosen).map(([m, a]) => `${m} ${a}`).join(', ')}\n`);
console.log('set              model                                   method   as asked @1   @8   vocab @1   @8   routing@4');
for (const r of table.filter(shown)) {
  if (r.error) {
    console.log(`${r.set.padEnd(16)} ${r.model.padEnd(39)} ERROR ${r.error}`);
    continue;
  }
  const v = r.perCategory?.vocabulary?.question;
  console.log(`${r.set.padEnd(16)} ${r.model.padEnd(39)} ${r.method.padEnd(8)} ${String(r.question['recall@1']).padStart(9)}% ${String(r.question['recall@8']).padStart(5)}%  ${v ? `${String(v['recall@1']).padStart(7)}% ${String(v['recall@8']).padStart(5)}%` : '      -      -'}  ${r.routing ? `${r.routing['recall@4']}%` : '-'}`);
}
console.log('\nmodel                                   $/M tokens   tokens   billed $   requests');
for (const m of MODELS) {
  const u = usage[m] || { tokens: 0, cost: 0, requests: 0 };
  console.log(`${m.padEnd(39)} ${String(prices[m] ?? '?').padStart(10)} ${String(u.tokens).padStart(8)} ${u.cost.toFixed(5).padStart(10)} ${String(u.requests).padStart(9)}${u.requests ? '' : '   (cached)'}`);
}
