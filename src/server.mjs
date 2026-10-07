// agent-wiki MCP server.
//   node server.mjs                     stdio (launched per client by the host app)
//   node server.mjs --http [--port N]   Streamable HTTP on 127.0.0.1 (run by the Windows service)
//       --parent-stdin                  exit gracefully when stdin closes (the service wrapper's stop signal)
//   node server.mjs --read-only         stdio with wiki_search and wiki_read only, writing nothing to the wiki
//                                       (the Ask agent's tools: ask.mjs)
//   node server.mjs --init              create the wiki skeleton, print {"ok":true,...}, exit
//   node server.mjs --version           print the version, exit
// In stdio mode stdout carries JSON-RPC only: all logging goes to stderr.
//
// Reads (wiki_start, wiki_search, wiki_read) are instant and deterministic.
// Writes (wiki_log, wiki_upsert_page) are queued as notes in inbox/ and filed
// by the curator (curator.mjs), unless writeMode is "direct" (v1.1 behavior).
// Every HTTP request and tool call is logged to logs/requests-YYYY-MM-DD.jsonl.

import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { z } from 'zod';
import * as ask from './ask.mjs';
import * as inbox from './inbox.mjs';
import { cleanupLocks, lockInfo } from './lock.mjs';
import { clip, createRequestLog, newRequestId, nullRequestLog, summarizeArgs } from './reqlog.mjs';
import * as ui from './ui-api.mjs';
import * as wiki from './wiki.mjs';

const log = (...a) => console.error(`[agent-wiki ${new Date().toISOString()}]`, ...a);

const READ_ONLY = { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false };
const WRITES = { readOnlyHint: false, destructiveHint: false, idempotentHint: false, openWorldHint: false };
const ASK_TOOLS = ['wiki_search', 'wiki_read'];
const MAX_BODY = 4 * 1024 * 1024;
const startedAt = Date.now();
/** Hash of this bundle, so the installer can tell the service is running exactly the build it just installed. */
const BUILD = (() => {
  try {
    return crypto.createHash('sha256').update(fs.readFileSync(fileURLToPath(import.meta.url))).digest('hex').slice(0, 12);
  } catch {
    return 'unknown';
  }
})();

const appParam = z
  .string()
  .describe('Which app or agent you are, e.g. "claude-desktop", "claude-code", "chatgpt-desktop", "codex".');
const keyParam = z
  .string()
  .optional()
  .describe('Optional. Any string unique to this note; sending the same key again never creates a duplicate.');

function leadIn(wikiDir, curated) {
  const writes = curated
    ? 'Tell it what happened with wiki_log (and wiki_upsert_page for page content): a curator agent organizes ' +
      'notes into pages, so hand over the facts and skip the formatting.'
    : 'Record decisions, outcomes, preferences, facts and follow-ups worth keeping with wiki_log, and keep topic ' +
      'pages current with wiki_upsert_page.';
  return (
    'Shared long-term memory for this user across Claude, ChatGPT and other AI apps ' +
    `(wiki folder: ${wiki.displayPath(wikiDir)}). Call wiki_start once at the beginning of EVERY conversation, ` +
    'before your first substantive reply, and follow the protocol it returns. Search the wiki (wiki_search) ' +
    `before asking the user for context they may have given before. ${writes} Never store secrets.`
  );
}

// ---------------------------------------------------------------- setup

async function setup() {
  const state = { wikiDir: null, setupError: null, config: {}, writeMode: 'curated' };
  try {
    state.config = await wiki.readConfig().catch(() => ({}));
    const mode = process.env.AGENT_WIKI_WRITE_MODE || state.config.writeMode;
    state.writeMode = mode === 'direct' ? 'direct' : 'curated';
    ({ wikiDir: state.wikiDir } = await wiki.resolveWikiDir());
    await wiki.ensureWiki(state.wikiDir);
    await cleanupLocks(state.wikiDir).catch(() => {});
    await wiki.withLock(state.wikiDir, () => wiki.refreshIndex(state.wikiDir));
  } catch (e) {
    state.setupError = e;
    log('setup failed:', e?.message || e);
  }
  return state;
}

/** setup() for --read-only: resolves the wiki and writes nothing (no skeleton, no lock cleanup, no index refresh). */
async function setupReadOnly() {
  const state = { wikiDir: null, setupError: null, config: {}, writeMode: 'curated', readOnly: true };
  try {
    state.config = await wiki.readConfig().catch(() => ({}));
    ({ wikiDir: state.wikiDir } = await wiki.resolveWikiDir());
    if (!(await wiki.exists(state.wikiDir))) throw new wiki.WikiError(`There is no wiki at ${wiki.displayPath(state.wikiDir)}.`);
  } catch (e) {
    state.setupError = e;
    log('setup failed:', e?.message || e);
  }
  return state;
}

function openRequestLog(proc, config) {
  try {
    return createRequestLog({
      dir: wiki.appPaths().logDir,
      proc,
      retentionDays: Number(config?.logs?.retentionDays) || 30,
      onError: (e) => log(`request log write failed: ${e.message}`),
    });
  } catch {
    return nullRequestLog;
  }
}

// ---------------------------------------------------------------- tools

/**
 * One McpServer per connection (stdio) or per request (HTTP).
 * ctx: {transport, rid?, client() -> "name version", reqlog, counters}
 */
async function createMcpServer(state, ctx) {
  const { wikiDir, setupError, writeMode, readOnly } = state;
  const curated = writeMode === 'curated';
  let instructions;
  if (readOnly) {
    instructions = `Read-only access to this user's shared wiki (${wiki.displayPath(wikiDir ?? '~/AgentWiki')}): wiki_search finds pages, log entries and notes not yet filed; wiki_read opens one.`;
  } else {
    let protocol = wiki.DEFAULT_PROTOCOL;
    if (wikiDir) protocol = await wiki.readProtocol(wikiDir).catch(() => wiki.DEFAULT_PROTOCOL);
    instructions = `${leadIn(wikiDir ?? '~/AgentWiki', curated)}\n\n${protocol.trim()}`;
  }
  const server = new McpServer({ name: 'agent-wiki', title: 'Agent Wiki', version: wiki.VERSION }, { instructions });
  ctx.server = server;

  // --read-only (the Ask agent) offers only the reads, so nothing it does can write to the wiki.
  const register = (name, ...rest) => (!readOnly || ASK_TOOLS.includes(name)) && server.registerTool(name, ...rest);

  // Every handler returns text; failures become isError results instead of crashing the server.
  // Each call is logged with a redacted summary of its arguments.
  const tool = (name, fn) => async (input) => {
    const t0 = Date.now();
    let result = 'ok';
    let error;
    try {
      if (!wikiDir || setupError) throw setupError ?? new wiki.WikiError('The wiki folder could not be resolved.');
      const text = await fn(input);
      return { content: [{ type: 'text', text }] };
    } catch (e) {
      const text = e instanceof wiki.WikiError ? e.message : `agent-wiki error: ${e?.message || e}`;
      result = e instanceof wiki.WikiError ? (/^Refused/.test(text) ? 'refused' : 'rejected') : 'error';
      error = text;
      if (!(e instanceof wiki.WikiError)) {
        log(e?.stack || e);
        ctx.counters?.error(text);
      }
      return { content: [{ type: 'text', text }], isError: true };
    } finally {
      ctx.reqlog.write({
        kind: 'tool',
        rid: ctx.rid || newRequestId(),
        transport: ctx.transport,
        client: clip(ctx.client(), 80),
        app: clip(input?.app, 40),
        ask: process.env.AGENT_WIKI_ASK || undefined,
        tool: name,
        args: summarizeArgs(input),
        ms: Date.now() - t0,
        result,
        error: clip(error, 300),
      });
    }
  };

  register(
    'wiki_start',
    {
      title: 'Start: load shared memory',
      description:
        'ALWAYS call this once at the start of every conversation, before your first substantive reply, in every app. ' +
        "Loads the user's shared long-term memory (shared across Claude, ChatGPT and other AI apps): the wiki protocol " +
        'to follow, the page index, the last 7 days of activity, notes not yet filed, and pages related to `topic`. ' +
        'Cheap and read-only. Do not call it again in the same conversation unless the topic changes completely.',
      inputSchema: {
        app: appParam,
        topic: z.string().optional().describe('A few words about what the user just asked, used to find related pages.'),
      },
      annotations: READ_ONLY,
    },
    tool('wiki_start', ({ app, topic }) => wiki.startContext(wikiDir, { app, topic })),
  );

  register(
    'wiki_search',
    {
      title: 'Search shared memory',
      description:
        "Full-text search of the user's shared wiki (topic pages, the daily activity log, and notes not yet filed). Use it " +
        'BEFORE asking the user for context they may have given before: project details, people, preferences, earlier ' +
        'decisions, where something lives. Returns ranked files with matching lines; open one with wiki_read.',
      inputSchema: {
        query: z.string().describe('Words to search for, e.g. "atlas docker networking".'),
        scope: z.enum(['all', 'pages', 'log']).optional().describe('Search pages, the log (with pending notes), or both (default all).'),
        limit: z.number().int().min(1).max(50).optional().describe('Maximum results (default 8).'),
      },
      annotations: READ_ONLY,
    },
    tool('wiki_search', async ({ query, scope, limit }) => {
      const hits = await wiki.search(wikiDir, query, { scope: scope || 'all', limit: limit || 8 });
      if (!hits.length) return `No results for "${query}". The wiki has nothing on this yet.`;
      return `${hits.length} result(s) for "${query}":\n\n${wiki.formatSearchResults(hits)}`;
    }),
  );

  register(
    'wiki_read',
    {
      title: 'Read a wiki page or log day',
      description:
        'Read one file from the shared wiki: a page slug (e.g. "atlas"), a log date ("YYYY-MM-DD"), or a path ' +
        'relative to the wiki folder (e.g. "index.md", "PROTOCOL.md", "pages/atlas.md", "inbox/<id>.md").',
      inputSchema: {
        target: z.string().describe('Page slug, log date YYYY-MM-DD, or relative path inside the wiki.'),
      },
      annotations: READ_ONLY,
    },
    tool('wiki_read', async ({ target }) => {
      const { rel, text } = await wiki.readTarget(wikiDir, target);
      return `File: ${rel}\n\n${text}`;
    }),
  );

  const queued = (r) =>
    r.duplicate
      ? `Already saved as note ${r.id} (${r.status}); nothing new was queued.`
      : `Saved note ${r.id} (${r.rel}). The curator will file it into the wiki shortly; until then it is listed under ` +
        '"Pending notes" in wiki_start and found by wiki_search. No need to check on it.';

  register(
    'wiki_log',
    {
      title: curated ? 'Tell the wiki what happened' : 'Log to shared memory',
      description: curated
        ? "Tell the user's shared wiki what happened. Hand over the facts quickly: a curator agent files them into the " +
          'right pages, links them and writes the activity log, so do not spend effort on structure or prose, and do ' +
          'not read pages first. Use when an exchange produced something a future session would want: a decision and ' +
          'why, an outcome (what was built, fixed, configured or learned, and where it lives), a stated preference or ' +
          'standing instruction, a fact about a project/person/system, or an open follow-up. Include names, paths, URLs ' +
          'and versions. One note per meaningful unit of work, usually at the end of the exchange. The note is saved ' +
          'durably at once. Never include secrets; say where a secret lives instead.'
        : "Append an entry to today's activity log in the user's shared wiki. Use when an exchange produced something a " +
          'future session would want: a decision and why, an outcome (something built, fixed, sent, configured or learned, ' +
          'and where it lives), a stated preference or standing instruction, a fact about a project/person/system, or an ' +
          'open follow-up. One entry per meaningful unit of work, usually at the end of the exchange, written to make sense ' +
          'without this conversation (name the project, include paths, URLs, versions). Never include secrets; write where ' +
          'a secret lives instead. Also update the matching page with wiki_upsert_page when durable knowledge changed.',
      inputSchema: {
        app: appParam,
        title: z.string().describe('One line: what happened, e.g. "Fixed Atlas docker networking".'),
        body: z.string().optional().describe('The details; plain notes are fine: what, why, where it lives, next steps.'),
        tags: z.array(z.string()).optional().describe('Short lowercase tags, e.g. ["atlas", "docker"].'),
        pages: z.array(z.string()).optional().describe('Slugs of related pages if you know them, e.g. ["atlas"].'),
        idempotency_key: keyParam,
      },
      annotations: WRITES,
    },
    tool('wiki_log', async (input) => {
      if (curated) return queued(await inbox.submitNote(wikiDir, { ...input, kind: 'log' }, { transport: ctx.transport, client: ctx.client() }));
      const { rel, heading } = await wiki.appendLog(wikiDir, input);
      return `Logged to ${rel}: ${heading.replace(/^## /, '')}`;
    }),
  );

  register(
    'wiki_upsert_page',
    {
      title: curated ? 'Suggest content for a wiki page' : 'Create or update a wiki page',
      description: curated
        ? 'Hand the shared wiki content for a page: one page per project, person, system, set of preferences, decision, ' +
          'how-to or reference. The curator merges it into the existing page (or creates it), keeping structure, links, ' +
          'history and superseded facts, so plain notes are fine. mode "replace" means your content is meant as the whole ' +
          'new page; "append" (default) means it adds to the page. Saved durably at once. Never include secrets.'
        : 'Create or update a page in the shared wiki: one page per project, person, system, set of preferences, decision, ' +
          'how-to or reference. Search and read the page first. mode "append" (default) adds a dated section; "replace" ' +
          'rewrites the whole page and keeps the old version in .history/. Keep `summary` to one line: it is what the ' +
          'index shows. Link related pages with [[slug]]. When facts change, say what changed. Regenerates index.md and ' +
          'logs the change. Never include secrets.',
      inputSchema: {
        app: appParam,
        title: z.string().describe('Page title, e.g. "Atlas". On append an existing title is kept.'),
        slug: z
          .string()
          .optional()
          .describe('Page id: lowercase letters, digits, hyphens (default: derived from title). Use an existing slug to update.'),
        type: z.enum(wiki.PAGE_TYPES).optional().describe('Page type (default "topic" for new pages).'),
        summary: z.string().optional().describe('One line describing what the page covers (shown in the index).'),
        tags: z.array(z.string()).optional().describe('Short lowercase tags.'),
        content: z.string().describe('Markdown. For append: the new information. For replace: the full new page body.'),
        mode: z.enum(['append', 'replace']).optional().describe('append (default) or replace.'),
        idempotency_key: keyParam,
      },
      annotations: WRITES,
    },
    tool('wiki_upsert_page', async (input) => {
      if (curated) {
        const note = {
          kind: 'page',
          app: input.app,
          title: input.title,
          body: input.content,
          tags: input.tags,
          pages: input.slug ? [input.slug] : [],
          page: { slug: input.slug, title: input.title, type: input.type, summary: input.summary, mode: input.mode },
          idempotency_key: input.idempotency_key,
        };
        return queued(await inbox.submitNote(wikiDir, note, { transport: ctx.transport, client: ctx.client() }));
      }
      const r = await wiki.upsertPage(wikiDir, input);
      const verb = { created: 'Created', updated: 'Appended to', replaced: 'Rewrote' }[r.action];
      return `${verb} ${r.rel}${r.historyRel ? ` (previous version kept at ${r.historyRel})` : ''}. index.md regenerated.`;
    }),
  );

  return server;
}

/** Recent errors (5xx, unexpected exceptions) for /status. */
function errorCounter() {
  const recent = [];
  let total = 0;
  let last = null;
  return {
    error(msg) {
      total++;
      last = { at: wiki.localISO(), message: clip(msg, 200) };
      recent.push(Date.now());
      while (recent.length && Date.now() - recent[0] > 15 * 60_000) recent.shift();
    },
    snapshot() {
      while (recent.length && Date.now() - recent[0] > 15 * 60_000) recent.shift();
      return { total, last15m: recent.length, last };
    },
  };
}

// ---------------------------------------------------------------- stdio

async function runStdio({ readOnly = false } = {}) {
  process.env.AGENT_WIKI_PROCESS ||= readOnly ? 'read-only' : 'stdio';
  const state = readOnly ? await setupReadOnly() : await setup();
  const reqlog = openRequestLog(readOnly ? process.env.AGENT_WIKI_PROCESS : 'stdio', state.config);
  const ctx = { transport: 'stdio', reqlog, client: () => '' };
  const server = await createMcpServer(state, ctx);
  ctx.client = () => {
    const v = server.server.getClientVersion();
    return v ? `${v.name} ${v.version}` : '';
  };
  server.server.oninitialized = () => reqlog.write({ kind: 'session', transport: 'stdio', client: clip(ctx.client(), 80), result: 'initialized' });
  await server.connect(new StdioServerTransport());
  log(`v${wiki.VERSION} ready (stdio, ${readOnly ? 'read-only' : `${state.writeMode} writes`}); wiki at ${wiki.displayPath(state.wikiDir ?? '(unavailable)')}`);
}

// ---------------------------------------------------------------- http

function sendJson(res, status, obj, headers = {}) {
  if (res.headersSent) return res.end();
  res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store', ...headers });
  res.end(`${JSON.stringify(obj)}\n`);
}

const rpcError = (code, message) => ({ jsonrpc: '2.0', error: { code, message }, id: null });

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > MAX_BODY) {
        reject(Object.assign(new Error('request body too large'), { status: 413 }));
        req.destroy();
      } else chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

/**
 * HTTP is stateless (every POST gets a fresh McpServer), so a tool call carries
 * no client name. We hand out an Mcp-Session-Id at initialize and remember its
 * clientInfo; clients echo the id on later requests. Unknown ids are never
 * rejected (a restarted service keeps serving old clients); the map is kept in
 * logs/http-sessions.json so names survive restarts.
 */
function sessionStore(file) {
  const map = new Map();
  try {
    for (const [k, v] of Object.entries(JSON.parse(fs.readFileSync(file, 'utf8')))) map.set(k, v);
  } catch {
    // none yet
  }
  let timer = null;
  const save = () => {
    timer = null;
    const obj = Object.fromEntries([...map.entries()].slice(-500));
    try {
      fs.mkdirSync(path.dirname(file), { recursive: true });
      const tmp = `${file}.${process.pid}.tmp`;
      fs.writeFileSync(tmp, JSON.stringify(obj));
      fs.renameSync(tmp, file);
    } catch {
      // best effort
    }
  };
  return {
    get: (id) => (id ? map.get(id) : undefined),
    set(id, client) {
      map.set(id, client);
      while (map.size > 500) map.delete(map.keys().next().value);
      timer ??= setTimeout(save, 200);
    },
  };
}

/** What a JSON-RPC body asks for, for the request log: "initialize", "tools/call wiki_log", ... */
function describeRpc(body) {
  const msgs = Array.isArray(body) ? body : [body];
  return msgs
    .map((m) => (m?.method === 'tools/call' ? `tools/call ${m.params?.name}` : m?.method || (m?.result !== undefined ? 'response' : '?')))
    .join(',');
}

async function statusReport(state, { port, counters }) {
  const s = {
    ok: !state.setupError,
    name: 'agent-wiki',
    version: wiki.VERSION,
    build: BUILD,
    pid: process.pid,
    startedAt: wiki.localISO(new Date(startedAt)),
    uptimeSec: Math.round((Date.now() - startedAt) / 1000),
    wikiDir: state.wikiDir && wiki.displayPath(state.wikiDir),
    mcpUrl: `http://127.0.0.1:${port}/mcp`,
    writeMode: state.writeMode,
    error: state.setupError?.message,
  };
  if (!state.wikiDir || state.setupError) return { ...s, health: 'degraded', reasons: [state.setupError?.message || 'wiki unavailable'] };
  const [queue, headlines, curator, paused, lock, asker] = await Promise.all([
    inbox.queueStats(state.wikiDir),
    wiki.recentHeadlines(state.wikiDir, 3, 10).catch(() => []),
    inbox.readCuratorStatus(state.wikiDir).catch(() => null),
    inbox.isPaused(state.wikiDir).catch(() => false),
    lockInfo(state.wikiDir).catch(() => null),
    ask.workerStatus(state.wikiDir).catch(() => ({ running: false })),
  ]);
  const heartbeatAge = curator?.heartbeatAt ? (Date.now() - Date.parse(curator.heartbeatAt)) / 1000 : null;
  const running = heartbeatAge !== null && heartbeatAge < 180;
  const errors = counters.snapshot();
  const reasons = [];
  const waitMin = queue.oldestPendingAt ? (Date.now() - Date.parse(queue.oldestPendingAt)) / 60_000 : 0;
  if (state.writeMode === 'curated') {
    if (queue.dead) reasons.push(`${queue.dead} note(s) failed curation`);
    if (queue.pending && waitMin > 30) reasons.push(`backlog: ${queue.pending} note(s), oldest waiting ${Math.round(waitMin)} min`);
    if (queue.pending && !running) reasons.push('curator is not running');
    if (running && curator.state === 'signed_out') reasons.push('curator is signed out of ChatGPT');
    if (running && ['rate_limited', 'error'].includes(curator.state)) reasons.push(`curator: ${curator.lastError || curator.state}`);
  }
  if (errors.last15m) reasons.push(`${errors.last15m} server error(s) in the last 15 min`);
  const lastNote = queue.newestAt ? Date.parse(queue.newestAt) : 0;
  const last = headlines[0];
  const lastLog = last ? Date.parse(`${last.date}T${last.time}:00`) : 0;
  return {
    ...s,
    health: reasons.length ? 'degraded' : 'ok',
    reasons,
    queue,
    lastWrite: lastNote > lastLog ? { at: queue.newestAt, text: 'note queued' } : last ? { at: `${last.date} ${last.time}`, text: last.text } : null,
    recent: headlines,
    curator: curator ? { ...curator, running, heartbeatAgeSec: heartbeatAge === null ? null : Math.round(heartbeatAge), paused } : { running: false, paused },
    requests: errors,
    ask: asker,
    lock: lock ? { state: lock.state, label: lock.owner?.label, pid: lock.owner?.pid, ageSec: Math.round(lock.ageMs / 1000) } : null,
  };
}

async function runHttp({ port, parentStdin }) {
  process.env.AGENT_WIKI_PROCESS ||= 'service';
  let state = await setup();
  const reqlog = openRequestLog('service', state.config);
  const sessions = sessionStore(wiki.appPaths().httpSessions);
  const counters = errorCounter();
  let inflight = 0;
  let closing = false;

  const handle = async (req, res, entry) => {
    // Only this machine, and never a web page: DNS-rebinding and cross-origin requests are refused.
    const actualPort = srv.address().port;
    const host = String(req.headers.host || '').toLowerCase();
    if (host !== `127.0.0.1:${actualPort}` && host !== `localhost:${actualPort}`) {
      entry.result = 'forbidden-host';
      return sendJson(res, 403, { error: 'Forbidden: unexpected Host header' });
    }
    const origin = req.headers.origin;
    if (origin && !/^https?:\/\/(?:127\.0\.0\.1|localhost)(?::\d+)?$/i.test(origin)) {
      entry.result = 'forbidden-origin';
      return sendJson(res, 403, { error: 'Forbidden: cross-origin request' });
    }
    const { pathname } = new URL(req.url, `http://${host}`);
    entry.path = pathname;
    if (pathname === '/health' || pathname === '/status') {
      if (req.method !== 'GET') return sendJson(res, 405, { error: 'Use GET' }, { Allow: 'GET' });
      if (state.setupError) state = await setup();
      if (pathname === '/status') {
        entry.quiet = true; // polled every few seconds by the tray: not worth a log line each time
        return sendJson(res, 200, await statusReport(state, { port: actualPort, counters }));
      }
      return sendJson(res, state.setupError ? 503 : 200, {
        ok: !state.setupError,
        name: 'agent-wiki',
        version: wiki.VERSION,
        build: BUILD,
        wikiDir: state.wikiDir && wiki.displayPath(state.wikiDir),
        pid: process.pid,
        uptimeSec: Math.round((Date.now() - startedAt) / 1000),
        writeMode: state.writeMode,
        error: state.setupError?.message,
      });
    }
    if (pathname === '/ui' || pathname.startsWith('/ui/') || pathname.startsWith('/api/')) {
      if (state.setupError) state = await setup();
      const url = new URL(req.url, `http://${host}`);
      return ui.handle(req, res, entry, { state, url, readBody, status: () => statusReport(state, { port: actualPort, counters }) });
    }
    if (pathname !== '/mcp') return sendJson(res, 404, { error: 'Not found: the MCP endpoint is /mcp, the tray window /ui/' });
    if (req.method !== 'POST') {
      return sendJson(res, 405, rpcError(-32000, 'Method not allowed: this server is stateless, use POST.'), { Allow: 'POST' });
    }
    if (closing) return sendJson(res, 503, rpcError(-32000, 'agent-wiki is restarting; retry shortly.'));
    let body;
    try {
      body = JSON.parse(await readBody(req));
    } catch (e) {
      entry.result = 'parse-error';
      return sendJson(res, e.status || 400, rpcError(-32700, `Parse error: ${e.message}`));
    }
    entry.rpc = describeRpc(body);
    let sid = String(req.headers['mcp-session-id'] || '') || undefined;
    const init = (Array.isArray(body) ? body : [body]).find((m) => m?.method === 'initialize');
    if (init) {
      sid = crypto.randomUUID();
      const ci = init.params?.clientInfo;
      sessions.set(sid, ci ? `${ci.name} ${ci.version ?? ''}`.trim() : 'unknown');
      res.setHeader('Mcp-Session-Id', sid);
    }
    entry.sid = sid;
    entry.client = sessions.get(sid);
    if (state.setupError) state = await setup(); // e.g. folder permissions fixed since startup
    const ctx = { transport: 'http', rid: entry.rid, reqlog, counters, client: () => sessions.get(sid) || '' };
    const server = await createMcpServer(state, ctx);
    const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
    res.on('close', () => {
      transport.close().catch(() => {});
      server.close().catch(() => {});
    });
    await server.connect(transport);
    await transport.handleRequest(req, res, body);
  };

  const srv = http.createServer((req, res) => {
    inflight++;
    const entry = { kind: 'http', rid: newRequestId(), transport: 'http', method: req.method, path: req.url, ua: clip(req.headers['user-agent'], 80) };
    const t0 = Date.now();
    res.on('close', () => {
      inflight--;
      if (entry.quiet && res.statusCode < 400) return;
      if (res.statusCode >= 500) counters.error(`HTTP ${res.statusCode} ${entry.rpc || entry.path}`);
      reqlog.write({ ...entry, quiet: undefined, client: clip(entry.client, 80), status: res.statusCode, ms: Date.now() - t0 });
    });
    handle(req, res, entry).catch((e) => {
      log('request failed:', e?.stack || e);
      entry.error = clip(e?.message || e, 300);
      sendJson(res, 500, rpcError(-32603, 'Internal error'));
    });
  });
  srv.keepAliveTimeout = 5000;

  const shutdown = async (reason) => {
    if (closing) return;
    closing = true;
    log(`shutting down (${reason})`);
    srv.close();
    srv.closeIdleConnections?.();
    const deadline = Date.now() + 5000;
    while (inflight > 0 && Date.now() < deadline) await new Promise((r) => setTimeout(r, 25));
    process.exit(0);
  };
  if (parentStdin) {
    process.stdin.on('error', () => {});
    process.stdin.on('data', () => {});
    process.stdin.on('end', () => shutdown('parent closed stdin'));
    process.stdin.on('close', () => shutdown('parent closed stdin'));
    process.stdin.resume();
  }
  process.on('SIGINT', () => shutdown('SIGINT'));
  process.on('SIGTERM', () => shutdown('SIGTERM'));

  srv.on('error', (e) => {
    log(`cannot listen on 127.0.0.1:${port}: ${e.message}`);
    process.exit(1);
  });
  srv.listen(port, '127.0.0.1', () => {
    log(
      `v${wiki.VERSION} listening on http://127.0.0.1:${srv.address().port}/mcp (${state.writeMode} writes); ` +
        `wiki at ${wiki.displayPath(state.wikiDir ?? '(unavailable)')}`,
    );
  });
}

// ---------------------------------------------------------------- main

async function main() {
  const args = process.argv.slice(2);
  const opt = (n) => {
    const i = args.indexOf(n);
    return i >= 0 ? args[i + 1] : undefined;
  };
  process.on('uncaughtException', (e) => log('uncaught:', e?.stack || e));
  process.on('unhandledRejection', (e) => log('unhandled rejection:', e?.stack || e));

  if (args.includes('--version')) {
    process.stdout.write(`${wiki.VERSION}\n`);
    return;
  }
  if (args.includes('--init')) {
    const state = await setup();
    const ok = !state.setupError;
    process.stdout.write(`${JSON.stringify({ ok, wikiDir: state.wikiDir, version: wiki.VERSION, writeMode: state.writeMode, error: state.setupError?.message })}\n`);
    process.exitCode = ok ? 0 : 1;
    return;
  }
  if (args.includes('--http')) {
    const cfg = await wiki.readConfig().catch(() => ({}));
    const port = Number(opt('--port') ?? cfg.httpPort ?? wiki.DEFAULT_HTTP_PORT);
    return runHttp({ port, parentStdin: args.includes('--parent-stdin') });
  }
  return runStdio({ readOnly: args.includes('--read-only') });
}

main().catch((e) => {
  log('fatal:', e?.stack || e);
  process.exit(1);
});
