// The activity log (log/YYYY/YYYY-MM-DD.md) as structured entries, for the tray window.

import { toLF } from './text.mjs';

const WIKILINK = /\[\[([a-z0-9][a-z0-9-]{0,79})(?:\|[^\]]*)?\]\]/g;

/** The page slugs a text links to with [[slug]] or [[slug|text]], in order, once each. */
export const linksIn = (text) => [...new Set([...String(text).matchAll(WIKILINK)].map((m) => m[1]))];

/**
 * One day of the log as entries, newest first: {date, time, app, title, tags, pages, body, compact}.
 * Full entries are "## HH:MM · app · title", then optional "tags:" and "pages:" lines before the
 * body; compact ones are "- HH:MM · app · text". Curator batch markers and the day heading are skipped.
 */
export function parseLogDay(date, text) {
  const out = [];
  let cur = null;
  const close = () => {
    if (!cur) return;
    cur.body = cur.lines.join('\n').trim();
    delete cur.lines;
    out.push(cur);
    cur = null;
  };
  for (const raw of toLF(text || '').split('\n')) {
    const line = raw.replace(/\s+$/, '');
    const full = line.match(/^## (\d{2}:\d{2}) · ([^·]+?) · (.+)$/);
    const compact = line.match(/^- (\d{2}:\d{2}) · ([^·]+?) · (.+)$/);
    if (full) {
      close();
      cur = { date, time: full[1], app: full[2].trim(), title: full[3].trim(), tags: [], pages: [], lines: [], compact: false };
      continue;
    }
    if (compact) {
      close();
      out.push({ date, time: compact[1], app: compact[2].trim(), title: compact[3].trim(), tags: [], pages: linksIn(compact[3]), body: '', compact: true });
      continue;
    }
    if (/^<!--.*-->$/.test(line.trim()) || /^# \d{4}-\d{2}-\d{2}$/.test(line)) {
      close();
      continue;
    }
    if (!cur) continue;
    // "tags:" and "pages:" lines come first, before any body text (after the heading's blank line).
    const head = !cur.lines.some((l) => l.trim());
    const tags = head && line.match(/^tags: (.+)$/);
    const pages = head && line.match(/^pages: (.+)$/);
    if (tags) cur.tags = tags[1].split(',').map((t) => t.trim()).filter(Boolean);
    else if (pages) cur.pages = linksIn(pages[1]);
    else cur.lines.push(line);
  }
  close();
  for (const e of out) if (!e.compact) e.pages = [...new Set([...e.pages, ...linksIn(e.body)])];
  return out.reverse();
}
