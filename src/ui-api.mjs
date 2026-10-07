// The tray window: a web app served at /ui/ (built from ui/ into runtime/ui) and the JSON API it
// reads, /api/*. Read-only apart from pausing the curator and asking questions (Ask: queued for
// the curator, see ask.mjs). Same protections as /mcp (the caller checks Host and Origin first);
// state-changing requests also need the X-Agent-Wiki header, which a page from another site cannot
// send without a preflight this server never grants.

import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { linksIn, parseLogDay } from './activity.mjs';
import * as ask from './ask.mjs';
import * as inbox from './inbox.mjs';
import * as wiki from './wiki.mjs';

/** runtime/ui next to the bundled server.mjs. */
export const UI_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), 'ui');

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.json': 'application/json',
  '.woff2': 'font/woff2',
};
const SECURITY = {
  'X-Content-Type-Options': 'nosniff',
  'Referrer-Policy': 'no-referrer',
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Content-Security-Policy':
    "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; " +
    "font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
};

function send(res, status, body, type = 'application/json', headers = {}) {
  if (res.headersSent) return res.end();
  res.writeHead(status, { 'Content-Type': type, 'Cache-Control': 'no-store', ...SECURITY, ...headers });
  res.end(body);
}
const json = (res, status, obj) => send(res, status, `${JSON.stringify(obj)}\n`);

// ---------------------------------------------------------------- the activity log

async function logDays(wikiDir, days, max) {
  const out = [];
  let count = 0;
  const now = new Date();
  for (let i = 0; i < days && count < max; i++) {
    const d = new Date(now.getFullYear(), now.getMonth(), now.getDate() - i, 12);
    const date = wiki.localDate(d);
    const text = await wiki.readIfExists(path.join(wikiDir, ...wiki.logRel(date).split('/')));
    if (!text) continue;
    const entries = parseLogDay(date, text).slice(0, max - count);
    count += entries.length;
    out.push({ date, entries });
  }
  return out;
}

// ---------------------------------------------------------------- pages

const pageSummary = (p) => ({
  slug: p.slug,
  title: p.title,
  type: p.type,
  summary: p.summary,
  tags: p.tags,
  updated: p.updated,
  updatedBy: String(p.meta.updated_by || ''),
  created: String(p.meta.created || ''),
  time: p.time,
  words: p.body.split(/\s+/).filter(Boolean).length,
});

async function pageHistory(wikiDir, slug) {
  const dir = path.join(wikiDir, '.history', 'pages', slug);
  const names = await fsp.readdir(dir).catch(() => []);
  return names.filter((n) => n.endsWith('.md')).sort().reverse().map((n) => ({ file: `.history/pages/${slug}/${n}`, at: n.slice(0, 19).replace(/_(\d\d)-(\d\d)-(\d\d)$/, 'T$1:$2:$3') }));
}

async function pageDetail(wikiDir, slug) {
  const pages = await wiki.listPages(wikiDir);
  const p = pages.find((x) => x.slug === slug);
  if (!p) return null;
  const backlinks = pages.filter((x) => x.slug !== slug && linksIn(x.body).includes(slug)).map((x) => ({ slug: x.slug, title: x.title, type: x.type }));
  const known = new Set(pages.map((x) => x.slug));
  return {
    ...pageSummary(p),
    body: wiki.stripMarkers(p.body).trim(),
    rel: p.rel,
    path: wiki.displayPath(path.join(wikiDir, p.rel)),
    links: linksIn(p.body).map((s) => ({ slug: s, exists: known.has(s), title: pages.find((x) => x.slug === s)?.title || s })),
    backlinks,
    history: await pageHistory(wikiDir, slug),
  };
}

// ---------------------------------------------------------------- the handler

const intParam = (v, d, lo, hi) => Math.max(lo, Math.min(hi, Number.parseInt(v, 10) || d));

/**
 * Handles /ui and /api/*. `ctx`: {state (wikiDir, setupError), url, status() -> the /status report,
 * readBody(req)}. Returns true when it answered.
 */
export async function handle(req, res, entry, ctx) {
  const { pathname, searchParams } = ctx.url;
  if (pathname === '/ui') return send(res, 308, '', 'text/plain', { Location: '/ui/' });
  if (pathname.startsWith('/ui/')) {
    if (req.method !== 'GET' && req.method !== 'HEAD') return send(res, 405, 'Use GET', 'text/plain', { Allow: 'GET' });
    const name = pathname === '/ui/' ? 'index.html' : pathname.slice(4);
    if (!/^[a-zA-Z0-9][a-zA-Z0-9._-]*$/.test(name)) return send(res, 404, 'Not found', 'text/plain');
    const file = path.join(UI_DIR, name);
    let body;
    try {
      body = await fsp.readFile(file);
    } catch {
      return send(res, 404, fs.existsSync(UI_DIR) ? 'Not found' : 'The tray window is not built: run npm run install-local.', 'text/plain');
    }
    entry.quiet = name !== 'index.html'; // one line per window opened, not per asset
    return send(res, 200, req.method === 'HEAD' ? '' : body, TYPES[path.extname(name).toLowerCase()] || 'application/octet-stream');
  }

  // /api/*
  if (pathname === '/api/status') {
    entry.quiet = true; // polled by the window
    return json(res, 200, await ctx.status());
  }
  if (ctx.state.setupError || !ctx.state.wikiDir) return json(res, 503, { error: ctx.state.setupError?.message || 'wiki not available' });
  const wikiDir = ctx.state.wikiDir;

  if (pathname === '/api/curator') {
    if (req.method !== 'POST') return json(res, 405, { error: 'Use POST' });
    if (req.headers['x-agent-wiki'] !== 'ui') return json(res, 403, { error: 'Forbidden: missing X-Agent-Wiki header' });
    let body;
    try {
      body = JSON.parse(await ctx.readBody(req));
    } catch {
      return json(res, 400, { error: 'Send JSON: {"paused": true|false}' });
    }
    if (typeof body?.paused !== 'boolean') return json(res, 400, { error: 'Send JSON: {"paused": true|false}' });
    await inbox.setPaused(wikiDir, body.paused);
    entry.result = body.paused ? 'curator paused' : 'curator resumed';
    return json(res, 200, { paused: await inbox.isPaused(wikiDir) });
  }
  if ((pathname === '/api/ask' || pathname === '/api/ask/cancel') && req.method === 'POST') {
    if (req.headers['x-agent-wiki'] !== 'ui') return json(res, 403, { error: 'Forbidden: missing X-Agent-Wiki header' });
    let body;
    try {
      body = JSON.parse(await ctx.readBody(req));
    } catch {
      return json(res, 400, { error: pathname === '/api/ask' ? 'Send JSON: {"question": "...", "parent": "<id>"?}' : 'Send JSON: {"id": "<id>"}' });
    }
    if (pathname === '/api/ask/cancel') {
      if (!ask.isAskId(body?.id)) return json(res, 400, { error: 'id: the id of a question' });
      const r = await ask.cancelAsk(wikiDir, body.id);
      entry.result = 'ask cancelled';
      return r ? json(res, 200, r) : json(res, 404, { error: 'No such question' });
    }
    // The model runs in the curator (as the user), never here: without a live worker nobody would answer.
    const worker = await ask.workerStatus(wikiDir);
    if (!worker.running) {
      return json(res, 503, { error: 'Asking needs the curator, which runs in the Agent Wiki tray app. Start the tray (Start menu > Agent Wiki), then ask again.', worker });
    }
    try {
      const r = await ask.createAsk(wikiDir, { question: typeof body?.question === 'string' ? body.question : '', parent: body?.parent });
      entry.result = 'ask queued';
      return json(res, 201, { ...r, worker });
    } catch (e) {
      if (e instanceof wiki.WikiError) return json(res, 400, { error: e.message });
      throw e;
    }
  }
  if (req.method !== 'GET') return json(res, 405, { error: 'Use GET' });

  if (pathname === '/api/ask') {
    entry.quiet = true; // polled while the agent works
    const id = String(searchParams.get('id') || '');
    if (!ask.isAskId(id)) return json(res, 400, { error: 'id: the id of a question' });
    const a = await ask.readAsk(wikiDir, id, { after: intParam(searchParams.get('after'), 0, 0, 100_000), thread: searchParams.get('thread') === '1' });
    return a ? json(res, 200, { ...a, worker: await ask.workerStatus(wikiDir) }) : json(res, 404, { error: 'No such question' });
  }
  if (pathname === '/api/asks') {
    entry.quiet = true;
    return json(res, 200, { asks: await ask.listAsks(wikiDir, { max: intParam(searchParams.get('max'), 30, 1, 100) }), worker: await ask.workerStatus(wikiDir) });
  }

  if (pathname === '/api/pages') {
    const pages = (await wiki.listPages(wikiDir)).map(pageSummary).sort((a, b) => b.time - a.time);
    return json(res, 200, { pages });
  }
  if (pathname === '/api/page') {
    const slug = String(searchParams.get('slug') || '');
    if (!wiki.SLUG_RE.test(slug)) return json(res, 400, { error: 'slug: lowercase letters, digits and hyphens' });
    const p = await pageDetail(wikiDir, slug);
    return p ? json(res, 200, p) : json(res, 404, { error: `No page "${slug}"` });
  }
  if (pathname === '/api/search') {
    const q = String(searchParams.get('q') || '').slice(0, 200);
    const scope = ['all', 'pages', 'log'].includes(searchParams.get('scope')) ? searchParams.get('scope') : 'all';
    const results = q.trim() ? await wiki.search(wikiDir, q, { scope, limit: intParam(searchParams.get('limit'), 20, 1, 50) }) : [];
    return json(res, 200, { query: q, terms: wiki.tokenize(q), results });
  }
  if (pathname === '/api/activity') {
    return json(res, 200, { days: await logDays(wikiDir, intParam(searchParams.get('days'), 14, 1, 90), intParam(searchParams.get('max'), 200, 1, 1000)) });
  }
  if (pathname === '/api/log') {
    const date = String(searchParams.get('date') || '');
    if (!wiki.DATE_RE.test(date)) return json(res, 400, { error: 'date: YYYY-MM-DD' });
    const text = await wiki.readIfExists(path.join(wikiDir, ...wiki.logRel(date).split('/')));
    return text === null ? json(res, 404, { error: `No log for ${date}` }) : json(res, 200, { date, entries: parseLogDay(date, text) });
  }
  if (pathname === '/api/inbox') {
    const notes = (await inbox.listNotes(wikiDir)).map((n) => ({
      id: n.id,
      app: n.app,
      kind: n.kind,
      title: n.title,
      body: n.body,
      submitted: n.submitted,
      pages: n.pages,
      tags: n.tags,
      status: n.status,
      attempts: n.attempts,
      lastError: n.lastError,
    }));
    return json(res, 200, { notes, paused: await inbox.isPaused(wikiDir) });
  }
  return json(res, 404, { error: 'Not found' });
}
