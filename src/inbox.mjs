// The inbox: notes the apps send ("tell the wiki what happened"). A note is
// accepted only once it is fsync'd to inbox/<id>.md; the curator organizes it
// into pages later. Notes stay visible (inbox/ is plain Markdown, wiki_start
// lists them, wiki_search finds them) until the curator has filed them.
//
//   inbox/<id>.md                      pending notes: frontmatter + the note
//   .curator/keys/<hash>.json          idempotency key -> note id
//   .curator/state/<id>.json           attempts, next retry, last error, dead
//   .curator/done/YYYY/MM/<id>.md      notes the curator has filed
//   .curator/audit/YYYY/MM/<id>.json   what the curator changed for that note, and why
//   .curator/journal/<batch>.json      write-ahead journal of a commit in progress
//   .curator/status.json               curator heartbeat (written by the curator)
//   .curator/paused                    present = curator paused
//
// Anyone may also drop a Markdown file into inbox/ by hand; it is treated as a note.

import crypto from 'node:crypto';
import fsp from 'node:fs/promises';
import path from 'node:path';
import {
  WikiError,
  assertNoSecrets,
  atomicWrite,
  hashText,
  historyStamp,
  localDate,
  localHM,
  localISO,
  normPageRefs,
  normTags,
  normalizeApp,
  oneLine,
  parseFrontmatter,
  readIfExists,
  serializePage,
  toLF,
  writeSynced,
  fsyncDir,
} from './wiki.mjs';

export const NOTE_KINDS = ['log', 'page'];

export function curatorPaths(wikiDir) {
  const cur = path.join(wikiDir, '.curator');
  return {
    inbox: path.join(wikiDir, 'inbox'),
    cur,
    keys: path.join(cur, 'keys'),
    state: path.join(cur, 'state'),
    done: path.join(cur, 'done'),
    audit: path.join(cur, 'audit'),
    journal: path.join(cur, 'journal'),
    tmp: path.join(cur, 'tmp'),
    status: path.join(cur, 'status.json'),
    paused: path.join(cur, 'paused'),
  };
}

const ID_RE = /^\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}-\d{3}-[a-z0-9]{6}$/;
export const newNoteId = (now = new Date()) => `${historyStamp(now)}-${crypto.randomBytes(4).toString('hex').slice(0, 6)}`;
const monthDir = (id) => (/^\d{4}-\d{2}/.test(id) ? path.join(id.slice(0, 4), id.slice(5, 7)) : 'undated');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function readJsonFile(file) {
  const t = await readIfExists(file);
  if (t === null) return null;
  try {
    return JSON.parse(t);
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------- submit

/** Canonical content of a note, for the derived idempotency key and the audit trail. */
function canonical(n) {
  return JSON.stringify([n.kind, n.app, n.title, n.body, n.tags, n.pages, n.page ?? null]);
}

/** Where a note id currently lives: 'inbox', 'done' or null. */
export async function noteLocation(wikiDir, id) {
  const p = curatorPaths(wikiDir);
  if (await readIfExists(path.join(p.inbox, `${id}.md`)) !== null) return 'inbox';
  if (await readIfExists(path.join(p.done, monthDir(id), `${id}.md`)) !== null) return 'done';
  return null;
}

/**
 * Validates, refuses secrets, deduplicates and durably queues a note.
 * input: {kind, app, title, body, tags, pages, page: {slug, title, type, summary, mode}, idempotency_key}
 * ctx:   {transport, client: "name version"}
 * Returns {id, rel, duplicate}.
 */
export async function submitNote(wikiDir, input, ctx = {}) {
  const kind = NOTE_KINDS.includes(input.kind) ? input.kind : 'log';
  const app = normalizeApp(input.app);
  const title = oneLine(input.title);
  if (!title) throw new WikiError('`title` is required.');
  const body = toLF(input.body).trim();
  if (kind === 'page' && !body) throw new WikiError('`content` is required.');
  const tags = normTags(input.tags);
  const pages = normPageRefs(input.pages);
  let page = null;
  if (kind === 'page') {
    const p = input.page || {};
    page = {
      slug: p.slug ? String(p.slug).trim().toLowerCase() : '',
      title: oneLine(p.title ?? title),
      type: p.type ? String(p.type).trim().toLowerCase() : '',
      summary: oneLine(p.summary ?? ''),
      mode: p.mode === 'replace' ? 'replace' : 'append',
    };
  }
  const key = input.idempotency_key == null ? '' : oneLine(input.idempotency_key).slice(0, 200);
  assertNoSecrets({ title, body, tags, pages, key, ...(page ? { slug: page.slug, summary: page.summary } : {}) });

  const now = new Date();
  const note = { kind, app, title, body, tags, pages, page };
  const keyHash = hashText(key ? `k:${app}:${key}` : `c:${localDate(now)}:${canonical(note)}`, 32);
  const p = curatorPaths(wikiDir);
  await fsp.mkdir(p.keys, { recursive: true });
  const keyFile = path.join(p.keys, `${keyHash}.json`);

  let id = null;
  for (let attempt = 0; attempt < 2 && !id; attempt++) {
    try {
      id = newNoteId(now);
      await writeSynced(keyFile, `${JSON.stringify({ id, at: localISO(now) })}\n`, 'wx');
      await fsyncDir(path.dirname(keyFile));
    } catch (e) {
      if (e.code !== 'EEXIST') throw e;
      id = null;
      let prev = await readJsonFile(keyFile);
      // The first submitter may still be writing the key file: give it a moment.
      for (let i = 0; i < 50 && !prev?.id; i++) {
        await sleep(20);
        prev = await readJsonFile(keyFile);
      }
      if (!prev?.id) {
        const st = await fsp.stat(keyFile).catch(() => null);
        if (st && Date.now() - st.mtimeMs < 10_000) throw new WikiError('The same note is being saved right now; try again in a moment.');
        await fsp.rm(keyFile, { force: true }); // unreadable and old: a crash while writing it
        continue;
      }
      // Same note sent again (a client retry, or the model calling twice). Wait briefly in case the first submit is still writing.
      for (let i = 0; i < 20; i++) {
        const where = await noteLocation(wikiDir, prev.id);
        if (where) return { id: prev.id, rel: `inbox/${prev.id}.md`, duplicate: true, status: where === 'done' ? 'filed' : 'pending' };
        await sleep(100);
      }
      const st = await fsp.stat(keyFile).catch(() => null);
      if (st && Date.now() - st.mtimeMs < 10_000) return { id: prev.id, rel: `inbox/${prev.id}.md`, duplicate: true, status: 'pending' };
      id = prev.id; // the first submit died between the key and the note: finish it with the same id
    }
  }
  if (!id) throw new WikiError('Could not queue the note (idempotency key conflict); try again.');

  const meta = {
    id,
    kind,
    app,
    title,
    tags,
    pages,
    submitted: localISO(now),
    ...(ctx.transport ? { transport: ctx.transport } : {}),
    ...(ctx.client ? { client: oneLine(ctx.client).slice(0, 80) } : {}),
    ...(key ? { idempotency_key: key } : {}),
    key: keyHash,
  };
  if (page) {
    meta.page_slug = page.slug;
    meta.page_title = page.title;
    meta.page_type = page.type;
    meta.page_summary = page.summary;
    meta.page_mode = page.mode;
  }
  await atomicWrite(path.join(p.inbox, `${id}.md`), serializePage(meta, body || title), { tmpDir: p.tmp });
  await faultPoint('submit-after-write');
  return { id, rel: `inbox/${id}.md`, duplicate: false, status: 'pending' };
}

/**
 * Test hook: AGENT_WIKI_TOUCH=<point> and AGENT_WIKI_TOUCH_FILE=<page file> append a line to that file
 * once, at that point, the way an editor saving a page at the worst moment would.
 */
let touched = false;
export async function touchPoint(name) {
  if (touched || process.env.AGENT_WIKI_TOUCH !== name || !process.env.AGENT_WIKI_TOUCH_FILE) return;
  touched = true;
  await fsp.appendFile(process.env.AGENT_WIKI_TOUCH_FILE, '\nSaved in an editor during the commit.\n');
}

/** Test hook: AGENT_WIKI_FAULT=<point> makes the process hang there so a test can kill it mid-operation. */
export async function faultPoint(name) {
  if (process.env.AGENT_WIKI_FAULT !== name) return;
  process.stderr.write(`[agent-wiki] FAULT ${name}: hanging\n`);
  await new Promise(() => {});
}

// ---------------------------------------------------------------- read

function noteFromText(id, text, mtimeMs) {
  const { meta, body } = parseFrontmatter(text);
  const submitted = String(meta.submitted || '');
  const d = Number.isNaN(Date.parse(submitted)) ? new Date(mtimeMs) : new Date(Date.parse(submitted));
  const heading = body.match(/^#\s+(.+)$/m)?.[1];
  const page =
    meta.kind === 'page'
      ? { slug: String(meta.page_slug || ''), title: String(meta.page_title || ''), type: String(meta.page_type || ''), summary: String(meta.page_summary || ''), mode: meta.page_mode === 'replace' ? 'replace' : 'append' }
      : null;
  return {
    id,
    rel: `inbox/${id}.md`,
    kind: meta.kind === 'page' ? 'page' : 'log',
    app: normalizeApp(meta.app || 'human'),
    title: oneLine(meta.title || heading || id),
    body: body.trim(),
    tags: normTags(meta.tags),
    pages: normPageRefs(meta.pages),
    page,
    client: meta.client ? String(meta.client) : '',
    submitted: localISO(d),
    date: localDate(d),
    time: localHM(d),
    ms: d.getTime(),
    mtimeMs,
    hash: hashText(text),
    byHand: !meta.id,
  };
}

/** Pending notes (oldest first), each merged with its queue state: status pending | retrying | dead. */
export async function listNotes(wikiDir) {
  const p = curatorPaths(wikiDir);
  let names;
  try {
    names = await fsp.readdir(p.inbox);
  } catch (e) {
    if (e.code === 'ENOENT') return [];
    throw e;
  }
  const notes = await Promise.all(
    names
      .filter((n) => n.endsWith('.md') && !n.startsWith('.'))
      .map(async (n) => {
        const id = n.slice(0, -3);
        const file = path.join(p.inbox, n);
        let text;
        let st;
        try {
          [text, st] = await Promise.all([fsp.readFile(file, 'utf8'), fsp.stat(file)]);
        } catch {
          return null; // filed or removed mid-read
        }
        const note = noteFromText(id, toLF(text), st.mtimeMs);
        const state = (await readJsonFile(path.join(p.state, `${id}.json`))) || {};
        note.attempts = state.attempts || 0;
        note.lastError = state.lastError || '';
        note.nextAt = state.nextAt || 0;
        note.isolate = Boolean(state.isolate);
        note.status = state.dead ? 'dead' : note.attempts ? 'retrying' : 'pending';
        return note;
      }),
  );
  return notes.filter(Boolean).sort((a, b) => a.ms - b.ms || a.id.localeCompare(b.id));
}

/** Pending notes as search documents (wiki_search scopes "all" and "log"). */
export async function noteSearchDocs(wikiDir) {
  return (await listNotes(wikiDir).catch(() => [])).map((n) => ({
    kind: 'note',
    rel: n.rel,
    label: `pending note from ${n.app}, ${n.date} ${n.time}`,
    slug: n.rel,
    meta: `${n.rel} ${n.title} ${n.tags.join(' ')} ${n.pages.join(' ')}`.toLowerCase(),
    summary: n.title,
    body: `${n.title}\n${n.body}`,
    time: n.ms,
  }));
}

// ---------------------------------------------------------------- queue state (curator side)

export async function readState(wikiDir, id) {
  return (await readJsonFile(path.join(curatorPaths(wikiDir).state, `${id}.json`))) || {};
}

export async function writeState(wikiDir, id, state) {
  const p = curatorPaths(wikiDir);
  await atomicWrite(path.join(p.state, `${id}.json`), `${JSON.stringify(state, null, 2)}\n`, { tmpDir: p.tmp });
}

export async function clearState(wikiDir, id) {
  await fsp.rm(path.join(curatorPaths(wikiDir).state, `${id}.json`), { force: true });
}

/** Moves a filed note to the archive and writes its audit record. Idempotent. */
export async function archiveNote(wikiDir, id, audit) {
  const p = curatorPaths(wikiDir);
  const dest = path.join(p.done, monthDir(id), `${id}.md`);
  const auditFile = path.join(p.audit, monthDir(id), `${id}.json`);
  if (audit) await atomicWrite(auditFile, `${JSON.stringify(audit, null, 2)}\n`, { tmpDir: p.tmp });
  await fsp.mkdir(path.dirname(dest), { recursive: true });
  const src = path.join(p.inbox, `${id}.md`);
  for (let i = 0; ; i++) {
    try {
      await fsp.rename(src, dest);
      await fsyncDir(path.dirname(dest));
      await fsyncDir(path.dirname(src));
      break;
    } catch (e) {
      if (e.code === 'ENOENT') break; // already archived (recovery re-run)
      if (i >= 20 || !['EPERM', 'EACCES', 'EBUSY'].includes(e.code)) throw e;
      await sleep(20 + i * 20);
    }
  }
  await clearState(wikiDir, id);
  return { done: path.relative(wikiDir, dest).replace(/\\/g, '/'), audit: audit ? path.relative(wikiDir, auditFile).replace(/\\/g, '/') : null };
}

export async function readAudit(wikiDir, id) {
  return readJsonFile(path.join(curatorPaths(wikiDir).audit, monthDir(id), `${id}.json`));
}

/** Resets dead (or all failed) notes so the curator tries them again. Returns how many. */
export async function retryNotes(wikiDir, { onlyDead = true } = {}) {
  let n = 0;
  for (const note of await listNotes(wikiDir)) {
    if (onlyDead ? note.status !== 'dead' : !note.attempts) continue;
    await clearState(wikiDir, note.id);
    n++;
  }
  return n;
}

export async function queueStats(wikiDir) {
  const notes = await listNotes(wikiDir).catch(() => []);
  const count = (s) => notes.filter((n) => n.status === s).length;
  return {
    pending: notes.length - count('dead'),
    retrying: count('retrying'),
    dead: count('dead'),
    oldestPendingAt: notes.find((n) => n.status !== 'dead')?.submitted ?? null,
    newestAt: notes.at(-1)?.submitted ?? null,
  };
}

// ---------------------------------------------------------------- curator status and pause flag

export async function readCuratorStatus(wikiDir) {
  return readJsonFile(curatorPaths(wikiDir).status);
}

export async function writeCuratorStatus(wikiDir, status) {
  const p = curatorPaths(wikiDir);
  await atomicWrite(p.status, `${JSON.stringify(status, null, 2)}\n`, { tmpDir: p.tmp });
}

export async function isPaused(wikiDir) {
  return (await readIfExists(curatorPaths(wikiDir).paused)) !== null;
}

export async function setPaused(wikiDir, paused) {
  const p = curatorPaths(wikiDir);
  if (paused) {
    await fsp.mkdir(p.cur, { recursive: true });
    await fsp.writeFile(p.paused, `paused at ${localISO()}\n`);
  } else await fsp.rm(p.paused, { force: true });
}

export const isNoteId = (id) => ID_RE.test(id);
