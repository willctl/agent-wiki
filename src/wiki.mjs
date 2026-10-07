// Storage layer for the Agent Wiki: config resolution, skeleton, locking,
// atomic writes, pages + frontmatter, the generated index, the daily log,
// full-text search, and the secret guard. No dependencies beyond Node.

import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import DEFAULT_PROTOCOL from '../protocol/PROTOCOL.md';
import { listNotes, noteSearchDocs } from './inbox.mjs';
import { LockBusyError, withLock as withRawLock } from './lock.mjs';
import { appPaths } from './paths.mjs';
import { findSecret } from './secrets.mjs';
import { MARKER_LINE, hashText, historyStamp, localDate, localHM, localISO, oneLine, stripMarkers, toLF } from './text.mjs';

export { findSecret, redactSecrets } from './secrets.mjs';
export { appPaths, pathsEnv } from './paths.mjs';
export * from './text.mjs';

export { DEFAULT_PROTOCOL };

// Replaced at build time by esbuild `define`.
export const VERSION = typeof __AGENT_WIKI_VERSION__ !== 'undefined' ? __AGENT_WIKI_VERSION__ : '0.0.0-dev';

export const SLUG_RE = /^[a-z0-9][a-z0-9-]{0,79}$/;
export const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
export const PAGE_TYPES = ['project', 'person', 'preference', 'decision', 'howto', 'reference', 'topic'];
const TYPE_HEADINGS = {
  project: 'Projects',
  person: 'People',
  preference: 'Preferences',
  decision: 'Decisions',
  howto: 'How-tos',
  reference: 'Reference',
  topic: 'Topics',
};

/** Errors whose message is meant for the model (returned as an isError tool result). */
export class WikiError extends Error {}

/** atomicWrite({expectHash}) found the file changed since it was read. */
export class StaleWriteError extends Error {}

// ---------------------------------------------------------------- config

export const DEFAULT_HTTP_PORT = 47821;

/**
 * The config.json in use: the standard one (src/paths.mjs), or, until `install-local` has moved it,
 * the one in the pre-1.3 ~/.agent-wiki.
 */
export async function configPath(env = process.env) {
  const p = appPaths({ env });
  if (p.mode === 'standard' && !env.AGENT_WIKI_CONFIG_DIR && !(await exists(p.configFile))) {
    const legacy = path.join(p.legacyHome, 'config.json');
    if (await exists(legacy)) return legacy;
  }
  return p.configFile;
}

/** config.json as an object ({} when missing). */
export async function readConfig(env = process.env) {
  const file = await configPath(env);
  let raw;
  try {
    raw = await fsp.readFile(file, 'utf8');
  } catch (e) {
    if (e.code === 'ENOENT') return {};
    throw e;
  }
  try {
    const cfg = JSON.parse(raw.replace(/^﻿/, ''));
    return cfg && typeof cfg === 'object' ? cfg : {};
  } catch (e) {
    throw new WikiError(`Agent Wiki config ${displayPath(file)} is not valid JSON: ${e.message}`);
  }
}

/**
 * Resolution order: env AGENT_WIKI_DIR > wikiDir in config.json (see configPath) > ~/AgentWiki.
 * Async on purpose: the session hook must never block its event loop on a slow or hung path.
 */
export async function resolveWikiDir(env = process.env) {
  if (env.AGENT_WIKI_DIR) return { wikiDir: path.resolve(env.AGENT_WIKI_DIR), source: 'AGENT_WIKI_DIR' };
  const cfg = await readConfig(env);
  if (typeof cfg.wikiDir === 'string' && cfg.wikiDir.trim()) {
    return { wikiDir: path.resolve(cfg.wikiDir), source: await configPath(env) };
  }
  return { wikiDir: path.join(os.homedir(), 'AgentWiki'), source: 'default' };
}

export const displayPath = (p) => String(p).replace(/\\/g, '/');

export function normalizeApp(app) {
  const s = String(app ?? '')
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9._-]+/g, '-')
    .replace(/-{2,}/g, '-')
    .replace(/^[-.]+|[-.]+$/g, '');
  return s.slice(0, 40) || 'unknown';
}

export function slugify(title) {
  return String(title ?? '')
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+/, '')
    .slice(0, 80)
    .replace(/-+$/, '');
}

export function normTags(tags) {
  if (tags == null || tags === '') return [];
  const list = Array.isArray(tags) ? tags : String(tags).split(',');
  const out = [];
  for (const t of list) {
    const v = oneLine(t).replace(/^#+/, '').trim().toLowerCase();
    if (v && !out.includes(v)) out.push(v);
  }
  return out;
}

export function normPageRefs(pages) {
  return normTags(pages)
    .map((p) => p.replace(/^\[\[|\]\]$/g, '').split('|')[0])
    .map((p) => (SLUG_RE.test(p) ? p : slugify(p)))
    .filter(Boolean);
}

// ---------------------------------------------------------------- secret guard

export function assertNoSecrets(fields) {
  for (const [name, value] of Object.entries(fields)) {
    const kind = findSecret(Array.isArray(value) ? value.join(' ') : value);
    if (kind) {
      throw new WikiError(
        `Refused: \`${name}\` looks like it contains ${kind}. Never store secrets in the wiki. ` +
          `Record WHERE the secret lives instead (for example "API key is in the 1Password vault 'Work'"), then try again.`,
      );
    }
  }
}

// ---------------------------------------------------------------- frontmatter

const PLAIN_SAFE = /^[A-Za-z0-9][A-Za-z0-9 _./()&+,-]*$/;
const YAML_RESERVED = /^(?:true|false|yes|no|on|off|null|y|n)$/i;
const NUMERIC = /^[-+]?(?:\d[\d_]*)?(?:\.\d+)?(?:e[-+]?\d+)?$/i;
const TIMESTAMP = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/;

/** A YAML value that is either a safe plain scalar or a JSON string/array (also valid YAML). */
function yamlValue(v) {
  if (Array.isArray(v)) return JSON.stringify(v.map(String));
  const s = String(v ?? '');
  if (TIMESTAMP.test(s)) return s;
  if (s && PLAIN_SAFE.test(s) && !YAML_RESERVED.test(s) && !NUMERIC.test(s) && !/\s$/.test(s)) return s;
  return JSON.stringify(s);
}

function parseValue(raw) {
  let s = raw.trim();
  if (!s) return '';
  if (s.startsWith('"')) {
    try {
      return JSON.parse(s);
    } catch {
      return s.replace(/^"|"$/g, '');
    }
  }
  if (s.startsWith("'")) return s.replace(/^'|'$/g, '').replace(/''/g, "'");
  if (s.startsWith('[')) {
    try {
      const a = JSON.parse(s);
      if (Array.isArray(a)) return a.map(String);
    } catch {
      // YAML flow sequence such as [a, 'b c'] written by hand
    }
    return s
      .replace(/^\[|\]$/g, '')
      .split(',')
      .map((x) => x.trim().replace(/^["']|["']$/g, ''))
      .filter(Boolean);
  }
  return s.replace(/\s+#.*$/, '');
}

export function parseFrontmatter(text) {
  const t = toLF(text).replace(/^﻿/, '');
  const lines = t.split('\n');
  if (lines[0].trim() !== '---') return { meta: {}, body: t };
  let close = -1;
  for (let i = 1; i < lines.length; i++) {
    if (/^(?:---|\.\.\.)\s*$/.test(lines[i])) {
      close = i;
      break;
    }
  }
  if (close < 0) return { meta: {}, body: t };
  const meta = {};
  let lastKey = null;
  for (const line of lines.slice(1, close)) {
    const kv = line.match(/^([A-Za-z0-9_-]+):(?:\s+(.*))?$/);
    if (kv) {
      lastKey = kv[1];
      meta[lastKey] = parseValue(kv[2] ?? '');
      continue;
    }
    const item = line.match(/^\s*-\s+(.*)$/);
    if (item && lastKey) {
      if (!Array.isArray(meta[lastKey])) meta[lastKey] = [];
      meta[lastKey].push(String(parseValue(item[1])));
    }
  }
  return { meta, body: lines.slice(close + 1).join('\n').replace(/^\n+/, '') };
}

const FM_ORDER = ['title', 'type', 'summary', 'tags', 'created', 'updated', 'updated_by'];

export function serializePage(meta, body) {
  const keys = [...FM_ORDER.filter((k) => k in meta), ...Object.keys(meta).filter((k) => !FM_ORDER.includes(k))];
  const fm = keys.map((k) => `${k}: ${yamlValue(meta[k])}`).join('\n');
  return `---\n${fm}\n---\n\n${toLF(body).trim()}\n`;
}

// ---------------------------------------------------------------- fs helpers

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function exists(p) {
  try {
    await fsp.stat(p);
    return true;
  } catch {
    return false;
  }
}

export async function readIfExists(file) {
  try {
    return toLF(await fsp.readFile(file, 'utf8'));
  } catch (e) {
    if (e.code === 'ENOENT') return null;
    throw e;
  }
}

/**
 * Makes a rename or a new file in `dir` durable: on POSIX that takes an fsync of the folder itself.
 * NTFS needs no such step (and Windows cannot open a folder this way), so it does nothing there.
 */
export async function fsyncDir(dir) {
  if (process.platform === 'win32') return;
  let fh;
  try {
    fh = await fsp.open(dir, 'r');
    await fh.sync();
  } catch {
    // best effort: some filesystems refuse to fsync a folder
  } finally {
    await fh?.close().catch(() => {});
  }
}

/** Writes and fsyncs a file in place (no rename). */
export async function writeSynced(file, content, flag = 'w') {
  const fh = await fsp.open(file, flag);
  try {
    await fh.writeFile(content, 'utf8');
    await fh.sync();
  } finally {
    await fh.close();
  }
}

/**
 * Temp file + fsync + rename, LF only, so readers see the old or the new file and
 * never a partial one, and a crash after return cannot lose the write. Retries
 * the rename because Windows briefly locks files (antivirus, indexer, editors).
 */
export async function atomicWrite(file, content, { tmpDir, expectHash } = {}) {
  await fsp.mkdir(path.dirname(file), { recursive: true });
  const base = `${path.basename(file)}.${process.pid}.${Math.random().toString(36).slice(2, 8)}.tmp`;
  if (tmpDir) await fsp.mkdir(tmpDir, { recursive: true });
  const tmp = path.join(tmpDir || path.dirname(file), base);
  await writeSynced(tmp, toLF(content));
  for (let i = 0; ; i++) {
    try {
      // Compare-then-swap for files people may edit without the lock: the check sits right before the
      // rename, after the slow write + fsync, so an editor's save can only slip in within microseconds.
      if (expectHash !== undefined) {
        const cur = await readIfExists(file);
        if ((cur === null ? null : hashText(cur)) !== expectHash) {
          await fsp.rm(tmp, { force: true }).catch(() => {});
          throw new StaleWriteError(`${displayPath(file)} changed just before it was written`);
        }
      }
      await fsp.rename(tmp, file);
      await fsyncDir(path.dirname(file));
      return;
    } catch (e) {
      if (e instanceof StaleWriteError) throw e;
      if (i >= 20 || !['EPERM', 'EACCES', 'EBUSY'].includes(e.code)) {
        await fsp.rm(tmp, { force: true }).catch(() => {});
        throw e;
      }
      await sleep(20 + i * 20);
    }
  }
}

export async function writeIfMissing(file, content) {
  try {
    await fsp.writeFile(file, toLF(content), { encoding: 'utf8', flag: 'wx' });
    await fsyncDir(path.dirname(file));
    return true;
  } catch (e) {
    if (e.code === 'EEXIST') return false;
    throw e;
  }
}

/**
 * Cross-process write lock (see lock.mjs): mkdir mutex, owner pid + start time,
 * immediate recovery when the owner is dead. A busy lock becomes a WikiError
 * so tools report it to the model instead of crashing.
 */
export async function withLock(wikiDir, fn, opts = {}) {
  try {
    return await withRawLock(wikiDir, fn, { label: process.env.AGENT_WIKI_PROCESS || 'agent-wiki', ...opts });
  } catch (e) {
    if (e instanceof LockBusyError) throw new WikiError(`The wiki is busy (${e.message}). Try again shortly.`);
    throw e;
  }
}

// ---------------------------------------------------------------- skeleton

const README = `# Agent Wiki

This folder is a shared, plain-Markdown memory used by AI apps on this PC
(Claude desktop, Claude Code, ChatGPT desktop, Codex) through the local
\`agent-wiki\` MCP server. You own it: open it in any editor or in Obsidian.

- \`PROTOCOL.md\`: the rules every AI session follows. Edit it freely; servers
  read it at startup and \`wiki_start\` returns the current text.
- \`index.md\`: generated from page frontmatter. Do not edit it by hand.
- \`pages/<slug>.md\`: one page per project, person, preference, decision,
  how-to, reference or topic.
- \`log/YYYY/YYYY-MM-DD.md\`: the daily, append-only activity log.
- \`.history/\`: earlier versions of pages that were rewritten.
- \`.locks/\`: transient write locks. Safe to delete when no app is running.
- \`inbox/\`: notes the apps sent that the curator has not organized yet.
- \`.curator/\`: the curator's queue state, journal, archive and audit trail.
`;

const WIKI_GITATTRIBUTES = '* text=auto eol=lf\n';
const WIKI_GITIGNORE = '.locks/\n*.tmp\n';

/** Creates any missing part of the skeleton; never overwrites user content. */
export async function ensureWiki(wikiDir) {
  await fsp.mkdir(wikiDir, { recursive: true });
  for (const d of ['pages', 'log', '.history', '.locks', 'inbox']) await fsp.mkdir(path.join(wikiDir, d), { recursive: true });
  await writeIfMissing(path.join(wikiDir, 'PROTOCOL.md'), DEFAULT_PROTOCOL);
  await writeIfMissing(path.join(wikiDir, 'README.md'), README);
  await writeIfMissing(path.join(wikiDir, '.gitattributes'), WIKI_GITATTRIBUTES);
  await writeIfMissing(path.join(wikiDir, '.gitignore'), WIKI_GITIGNORE);
  if (!(await exists(path.join(wikiDir, 'index.md')))) await refreshIndex(wikiDir);
}

export async function readProtocol(wikiDir) {
  return (await readIfExists(path.join(wikiDir, 'PROTOCOL.md'))) ?? DEFAULT_PROTOCOL;
}

// ---------------------------------------------------------------- pages + index

function firstHeading(body) {
  const m = body.match(/^#\s+(.+)$/m);
  return m ? m[1].trim() : '';
}

export async function listPages(wikiDir) {
  let names;
  try {
    names = await fsp.readdir(path.join(wikiDir, 'pages'));
  } catch (e) {
    if (e.code === 'ENOENT') return [];
    throw e;
  }
  const pages = await Promise.all(
    names
      .filter((n) => n.endsWith('.md'))
      .map(async (n) => {
        const file = path.join(wikiDir, 'pages', n);
        let text;
        let mtimeMs;
        try {
          [text, { mtimeMs }] = await Promise.all([fsp.readFile(file, 'utf8'), fsp.stat(file)]);
        } catch {
          return null; // deleted or replaced mid-read
        }
        const { meta, body } = parseFrontmatter(text);
        const slug = n.slice(0, -3);
        return {
          slug,
          rel: `pages/${n}`,
          title: oneLine(meta.title || firstHeading(body) || slug),
          type: String(meta.type || 'topic').toLowerCase(),
          summary: oneLine(meta.summary || ''),
          tags: normTags(meta.tags),
          updated: String(meta.updated || ''),
          time: Date.parse(meta.updated) || mtimeMs,
          meta,
          body,
        };
      }),
  );
  return pages.filter(Boolean).sort((a, b) => a.slug.localeCompare(b.slug));
}

const linkText = (s) => s.replace(/\|/g, '/').replace(/\]\]/g, ')');

export function renderIndex(pages) {
  const groups = new Map();
  for (const p of pages) {
    if (!groups.has(p.type)) groups.set(p.type, []);
    groups.get(p.type).push(p);
  }
  const order = [
    ...PAGE_TYPES.filter((t) => groups.has(t)),
    ...[...groups.keys()].filter((t) => !PAGE_TYPES.includes(t)).sort(),
  ];
  const out = [
    '<!-- GENERATED by agent-wiki from page frontmatter. Do not edit: it is rewritten on every page change. -->',
    '',
    '# Index',
    '',
  ];
  if (!pages.length) out.push('_No pages yet._', '');
  for (const t of order) {
    out.push(`## ${TYPE_HEADINGS[t] || t[0].toUpperCase() + t.slice(1)}`, '');
    const list = groups.get(t).sort((a, b) => a.title.localeCompare(b.title));
    for (const p of list) out.push(`- [[${p.slug}|${linkText(p.title)}]]${p.summary ? ` - ${p.summary}` : ''}`);
    out.push('');
  }
  return out.join('\n');
}

/** Regenerates index.md, writing only if it changed. */
export async function refreshIndex(wikiDir, pages) {
  const next = renderIndex(pages ?? (await listPages(wikiDir)));
  const file = path.join(wikiDir, 'index.md');
  const cur = await readIfExists(file);
  if (cur === toLF(next).replace(/\n*$/, '\n')) return false;
  await atomicWrite(file, next.replace(/\n*$/, '\n'));
  return true;
}

export async function upsertPage(wikiDir, input) {
  const app = normalizeApp(input.app);
  const title = oneLine(input.title);
  if (!title) throw new WikiError('`title` is required.');
  const slug = input.slug ? String(input.slug).trim().toLowerCase() : slugify(title);
  if (!SLUG_RE.test(slug)) {
    throw new WikiError(`Invalid slug "${slug}". Use 1-80 lowercase letters, digits and hyphens, e.g. "atlas".`);
  }
  const mode = input.mode || 'append';
  if (mode !== 'append' && mode !== 'replace') throw new WikiError('`mode` must be "append" or "replace".');
  const type = input.type ? String(input.type).trim().toLowerCase() : undefined;
  if (type && !/^[a-z][a-z0-9-]{0,30}$/.test(type)) throw new WikiError(`Invalid type "${type}".`);
  const summary = input.summary == null ? undefined : oneLine(input.summary);
  const tags = input.tags == null ? undefined : normTags(input.tags);
  const content = toLF(input.content).trim();
  if (!content) throw new WikiError('`content` is required.');
  assertNoSecrets({ title, summary, tags, content, slug });

  return withLock(wikiDir, async () => {
    const now = new Date();
    const stamp = localISO(now);
    const file = path.join(wikiDir, 'pages', `${slug}.md`);
    const existing = await readIfExists(file);
    let action;
    let meta;
    let body;
    let historyRel = null;
    if (existing === null) {
      action = 'created';
      meta = { title, type: type || 'topic', summary: summary || '', tags: tags || [], created: stamp, updated: stamp, updated_by: app };
      body = /^#\s/.test(content) ? content : `# ${title}\n\n${content}`;
    } else {
      const parsed = parseFrontmatter(existing);
      meta = { ...parsed.meta };
      if (type) meta.type = type;
      meta.type = meta.type || 'topic';
      if (summary) meta.summary = summary;
      meta.summary = meta.summary ?? '';
      if (mode === 'replace') {
        action = 'replaced';
        const dir = path.join(wikiDir, '.history', 'pages', slug);
        await fsp.mkdir(dir, { recursive: true });
        const base = historyStamp(now);
        for (let i = 1; ; i++) {
          const name = i === 1 ? `${base}.md` : `${base}-${i}.md`;
          if (await writeIfMissing(path.join(dir, name), existing)) {
            historyRel = `.history/pages/${slug}/${name}`;
            break;
          }
        }
        meta.title = title;
        meta.tags = tags ?? normTags(meta.tags);
        body = /^#\s/.test(content) ? content : `# ${title}\n\n${content}`;
      } else {
        action = 'updated';
        meta.title = meta.title || title;
        meta.tags = [...new Set([...normTags(meta.tags), ...(tags || [])])];
        body = `${parsed.body.trimEnd()}\n\n### ${localDate(now)} ${localHM(now)} (${app})\n\n${content}`;
      }
      meta.created = meta.created || stamp;
      meta.updated = stamp;
      meta.updated_by = app;
    }
    await atomicWrite(file, serializePage(meta, body));
    await refreshIndex(wikiDir);
    const verb = { created: 'Page created', updated: 'Page updated', replaced: 'Page rewritten' }[action];
    await appendToLog(wikiDir, now, `- ${localHM(now)} · ${app} · ${verb}: ${oneLine(meta.title)} [[${slug}]]`, true);
    return { action, slug, rel: `pages/${slug}.md`, historyRel };
  });
}

// ---------------------------------------------------------------- log

export const logRel = (date) => `log/${date.slice(0, 4)}/${date}.md`;
const COMPACT_LINE = /^- \d{2}:\d{2} · /;

export async function appendToLog(wikiDir, now, chunk, compact = false) {
  const date = localDate(now);
  const file = path.join(wikiDir, ...logRel(date).split('/'));
  let text = (await readIfExists(file)) ?? '';
  if (!text.trim()) text = `# ${date}\n`;
  const lastLine = text.trimEnd().split('\n').pop();
  const sep = compact && COMPACT_LINE.test(lastLine) ? '\n' : '\n\n';
  await atomicWrite(file, `${text.trimEnd()}${sep}${chunk.trim()}\n`);
  return logRel(date);
}

export async function appendLog(wikiDir, input) {
  const app = normalizeApp(input.app);
  const title = oneLine(input.title);
  if (!title) throw new WikiError('`title` is required.');
  const body = toLF(input.body).trim();
  const tags = normTags(input.tags);
  const pages = normPageRefs(input.pages);
  assertNoSecrets({ title, body, tags, pages });
  return withLock(wikiDir, async () => {
    const now = new Date();
    const heading = `## ${localHM(now)} · ${app} · ${title}`;
    const metaLines = [];
    if (tags.length) metaLines.push(`tags: ${tags.join(', ')}`);
    if (pages.length) metaLines.push(`pages: ${pages.map((p) => `[[${p}]]`).join(', ')}`);
    let chunk = heading;
    if (metaLines.length) chunk += `\n\n${metaLines.join('  \n')}`;
    if (body) chunk += `\n\n${body}`;
    const rel = await appendToLog(wikiDir, now, chunk);
    return { rel, heading };
  });
}

async function readLogDay(wikiDir, date) {
  return readIfExists(path.join(wikiDir, ...logRel(date).split('/')));
}

function daysBack(n, now = new Date()) {
  const out = [];
  for (let i = 0; i < n; i++) {
    const d = new Date(now.getFullYear(), now.getMonth(), now.getDate() - i, 12);
    out.push(localDate(d));
  }
  return out; // newest first
}

/** Last `days` of log, chronological, truncated from the oldest end to `cap` chars. */
export async function recentLog(wikiDir, days = 7, cap = 5000) {
  const dates = daysBack(days).reverse();
  const texts = await Promise.all(dates.map((d) => readLogDay(wikiDir, d)));
  let joined = texts.filter((t) => t && t.trim()).map((t) => stripMarkers(t).trim()).join('\n\n');
  if (joined.length <= cap) return joined;
  let cut = joined.slice(joined.length - cap);
  const boundary = cut.search(/\n(?=#{1,2} |- \d{2}:\d{2} · )/);
  if (boundary >= 0) cut = cut.slice(boundary + 1);
  return `(older entries truncated; read a day with wiki_read("YYYY-MM-DD"))\n\n${cut}`;
}

/** Newest log headlines first: [{date, time, text}]. */
export async function recentHeadlines(wikiDir, days = 3, max = 15) {
  const out = [];
  for (const date of daysBack(days)) {
    const text = await readLogDay(wikiDir, date);
    if (!text) continue;
    const day = [];
    for (const line of text.split('\n')) {
      const m = line.match(/^(?:## |- )(\d{2}:\d{2}) · (.+)$/);
      if (m) day.push({ date, time: m[1], text: m[2].trim() });
    }
    out.push(...day.reverse());
    if (out.length >= max) break;
  }
  return out.slice(0, max);
}

// ---------------------------------------------------------------- search

const STOPWORDS = new Set(
  ('a an and are as at be by can could did do does for from had has have how i in into is it its me my of on or ' +
    'our should so than that the their them then there these this those to was we were what when where which who ' +
    'why will with would you your about any all').split(' '),
);

export function tokenize(query) {
  const raw = (toLF(query).toLowerCase().match(/[\p{L}\p{N}][\p{L}\p{N}._/-]*/gu) || [])
    .map((t) => t.replace(/[._/-]+$/, ''))
    .filter((t) => t.length > 1 || /\d/.test(t));
  const uniq = [...new Set(raw)];
  const kept = uniq.filter((t) => !STOPWORDS.has(t));
  return kept.length ? kept : uniq;
}

function countOccurrences(hay, needle) {
  let n = 0;
  for (let i = hay.indexOf(needle); i !== -1 && n < 50; i = hay.indexOf(needle, i + needle.length)) n++;
  return n;
}

async function listLogFiles(wikiDir) {
  const base = path.join(wikiDir, 'log');
  let years;
  try {
    years = await fsp.readdir(base);
  } catch (e) {
    if (e.code === 'ENOENT') return [];
    throw e;
  }
  const out = [];
  for (const y of years.filter((y) => /^\d{4}$/.test(y))) {
    for (const f of await fsp.readdir(path.join(base, y)).catch(() => [])) {
      const m = f.match(/^(\d{4}-\d{2}-\d{2})\.md$/);
      if (m) out.push({ date: m[1], rel: `log/${y}/${f}`, file: path.join(base, y, f) });
    }
  }
  return out;
}

async function loadDocs(wikiDir, scope) {
  const docs = [];
  if (scope !== 'log') {
    for (const p of await listPages(wikiDir)) {
      docs.push({
        kind: 'page',
        rel: p.rel,
        label: `${p.title} [${p.type}]`,
        slug: p.slug,
        meta: `${p.rel} ${p.slug} ${p.title} ${p.summary} ${p.tags.join(' ')}`.toLowerCase(),
        summary: p.summary,
        body: p.body,
        time: p.time,
      });
    }
  }
  if (scope !== 'pages') {
    docs.push(...(await noteSearchDocs(wikiDir)));
    const logs = await listLogFiles(wikiDir);
    const texts = await Promise.all(logs.map((l) => readIfExists(l.file)));
    logs.forEach((l, i) => {
      if (!texts[i]) return;
      docs.push({
        kind: 'log',
        rel: l.rel,
        label: `log ${l.date}`,
        slug: l.date,
        meta: `${l.rel} ${l.date}`.toLowerCase(),
        summary: '',
        body: texts[i],
        time: Date.parse(`${l.date}T23:59:59`) || 0,
      });
    });
  }
  return docs;
}

function snippetLines(body, terms, max = 3) {
  const out = [];
  let heading = '';
  for (const line of body.split('\n')) {
    const h = line.match(/^#{2,3}\s+(.+)$/);
    if (h) heading = h[1].trim();
    if (!line.trim() || MARKER_LINE.test(line.trim())) continue;
    const lower = line.toLowerCase();
    if (!terms.some((t) => lower.includes(t))) continue;
    const text = line.trim().length > 220 ? `${line.trim().slice(0, 217)}...` : line.trim();
    out.push(heading && !h ? `[${heading}] ${text}` : text);
    if (out.length >= max) break;
  }
  return out;
}

export async function search(wikiDir, query, { scope = 'all', limit = 8 } = {}) {
  const terms = tokenize(query);
  if (!terms.length) return [];
  const phrase = toLF(query).toLowerCase().replace(/\s+/g, ' ').trim();
  const now = Date.now();
  const results = [];
  for (const doc of await loadDocs(wikiDir, scope)) {
    const hay = doc.body.toLowerCase();
    let score = 0;
    let matched = 0;
    for (const t of terms) {
      const c = countOccurrences(hay, t);
      const inMeta = doc.meta.includes(t);
      if (c || inMeta) matched++;
      score += Math.min(c, 8);
      if (inMeta) score += 6;
    }
    if (!matched) continue;
    if (terms.length > 1 && phrase.length > 3 && (hay.includes(phrase) || doc.meta.includes(phrase))) score += 8;
    score *= matched / terms.length;
    if (doc.kind === 'page') score *= 1.3;
    const ageDays = Math.max(0, (now - doc.time) / 86_400_000);
    score *= 1 + 0.3 * Math.exp(-ageDays / 14);
    const lines = snippetLines(doc.body, terms);
    if (!lines.length && doc.summary) lines.push(doc.summary);
    results.push({ kind: doc.kind, rel: doc.rel, target: doc.slug, label: doc.label, score: Math.round(score * 10) / 10, snippets: lines });
  }
  results.sort((a, b) => b.score - a.score || b.rel.localeCompare(a.rel));
  return results.slice(0, Math.max(1, Math.min(Number(limit) || 8, 50)));
}

export function formatSearchResults(results) {
  return results
    .map((r, i) => {
      const head = `${i + 1}. ${r.rel} - ${r.label} (read: "${r.target}", score ${r.score})`;
      return [head, ...r.snippets.map((s) => `   > ${s}`)].join('\n');
    })
    .join('\n');
}

// ---------------------------------------------------------------- read

function isInside(root, p) {
  const rel = path.relative(root, p);
  return rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel));
}

/** `abs` with its last part matched case-insensitively in its folder (case-sensitive filesystems), or null. */
async function caseInsensitive(abs) {
  const dir = path.dirname(abs);
  const want = path.basename(abs).toLowerCase();
  const names = await fsp.readdir(dir).catch(() => []);
  const hit = names.find((n) => n.toLowerCase() === want) ?? names.find((n) => n.toLowerCase() === `${want}.md`);
  return hit ? path.join(dir, hit) : null;
}

/** Accepts a log date, a page slug, [[slug]], or a path relative to the wiki root. */
export async function readTarget(wikiDir, target) {
  let t = String(target ?? '').trim().replace(/\\/g, '/');
  t = t.replace(/^\[\[|\]\]$/g, '').split('|')[0].trim();
  if (!t) throw new WikiError('`target` is required: a page slug, a log date (YYYY-MM-DD) or a path.');
  const root = path.resolve(wikiDir);
  let rel;
  // A date also matches the slug pattern, so test it first.
  if (DATE_RE.test(t)) {
    rel = logRel(t);
    if (!(await exists(path.join(root, rel)))) throw new WikiError(`No log entries for ${t}.`);
  } else if (SLUG_RE.test(t) && (await exists(path.join(root, 'pages', `${t}.md`)))) {
    rel = `pages/${t}.md`;
  } else if (SLUG_RE.test(t.toLowerCase()) && (await exists(path.join(root, 'pages', `${t.toLowerCase()}.md`)))) {
    rel = `pages/${t.toLowerCase()}.md`; // "Agent-Wiki": slugs are lowercase
  } else {
    rel = t;
  }
  let abs = path.resolve(root, rel);
  if (!isInside(root, abs)) throw new WikiError(`Refused: "${target}" is outside the wiki folder.`);
  if (!(await exists(abs)) && !/\.md$/i.test(abs) && (await exists(`${abs}.md`))) abs = `${abs}.md`;
  if (!(await exists(abs))) abs = (await caseInsensitive(abs)) ?? abs; // Linux: "INDEX.md", "Protocol.md"
  if (!(await exists(abs))) {
    throw new WikiError(`Not found: "${target}". Use wiki_search, or a page slug from the index (wiki_read("index.md")).`);
  }
  const realRoot = await fsp.realpath(root);
  const real = await fsp.realpath(abs);
  if (!isInside(realRoot, real)) throw new WikiError(`Refused: "${target}" resolves outside the wiki folder.`);
  const relOut = displayPath(path.relative(root, abs)) || '.';
  const st = await fsp.stat(abs);
  if (st.isDirectory()) {
    const entries = (await fsp.readdir(abs, { withFileTypes: true }))
      .filter((e) => !e.name.startsWith('.') || relOut === '.')
      .map((e) => (e.isDirectory() ? `${e.name}/` : e.name))
      .sort();
    return { rel: relOut, text: `Directory ${relOut}/:\n${entries.map((e) => `- ${e}`).join('\n')}` };
  }
  return { rel: relOut, text: toLF(await fsp.readFile(abs, 'utf8')) };
}

// ---------------------------------------------------------------- wiki_start

export async function startContext(wikiDir, { app, topic } = {}) {
  const now = new Date();
  const [protocol, pages, recent, notes] = await Promise.all([
    readProtocol(wikiDir),
    listPages(wikiDir),
    recentLog(wikiDir, 7, 5000),
    listNotes(wikiDir).catch(() => []),
  ]);
  const weekday = now.toLocaleDateString('en-US', { weekday: 'long' });
  const out = [
    '# Agent Wiki: session start',
    '',
    `Now: ${localISO(now)} (${weekday}). Wiki folder: ${displayPath(wikiDir)}. You are: ${normalizeApp(app)}.`,
    '',
    '---',
    '',
    protocol.trim(),
    '',
    '---',
    '',
    `## Page index (${pages.length})`,
    '',
  ];
  if (pages.length) for (const p of pages) out.push(`- ${p.slug} [${p.type}] ${p.title}${p.summary ? ` - ${p.summary}` : ''}`);
  else out.push('(no pages yet)');
  const t = oneLine(topic ?? '');
  if (t) {
    out.push('', `## Related to "${t}"`, '');
    const hits = await search(wikiDir, t, { limit: 5 });
    out.push(hits.length ? formatSearchResults(hits) : '(nothing related found)');
  }
  if (notes.length) out.push('', ...renderPendingNotes(notes));
  out.push('', '## Recent activity (last 7 days)', '', recent || '(no activity logged yet)');
  out.push('', 'Read a page with wiki_read("<slug>") or a day with wiki_read("YYYY-MM-DD").');
  return out.join('\n');
}

const STATUS_LABEL = { pending: 'pending', retrying: 'retrying', dead: 'NOT CURATED: failed, see the tray' };

/** wiki_start section: notes the curator has not organized yet. Their text is shown so nothing is invisible meanwhile. */
export function renderPendingNotes(notes, { max = 20, cap = 4000 } = {}) {
  const out = [`## Pending notes (${notes.length}, sent by apps, not yet organized into pages)`, ''];
  let used = 0;
  for (const n of notes.slice(0, max)) {
    const body = oneLine(n.body);
    const snippet = body.length > 240 ? `${body.slice(0, 237)}...` : body;
    const line = `- ${n.date} ${n.time} · ${n.app} · ${n.title || '(untitled)'} [${STATUS_LABEL[n.status] || n.status}]${snippet ? `: ${snippet}` : ''}`;
    if (used + line.length > cap) break;
    used += line.length;
    out.push(line);
  }
  if (notes.length > max) out.push(`- ...and ${notes.length - max} more`);
  out.push('', 'Read one in full with wiki_read("inbox/<id>.md"). Do not send them again: they are already saved.');
  return out;
}
