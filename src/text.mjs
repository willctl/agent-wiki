// Time and text helpers shared by every module (no dependencies, importable unbundled).

import crypto from 'node:crypto';

const pad = (n, w = 2) => String(n).padStart(w, '0');

/** Local time with offset, e.g. 2026-10-01T13:42:05-05:00. */
export function localISO(d = new Date()) {
  const off = -d.getTimezoneOffset();
  const a = Math.abs(off);
  return `${localDate(d)}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}` +
    `${off >= 0 ? '+' : '-'}${pad(Math.floor(a / 60))}:${pad(a % 60)}`;
}
export const localDate = (d = new Date()) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
export const localHM = (d = new Date()) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
/** Windows-safe (no colons) stamp for .history file names and note ids. */
export const historyStamp = (d) =>
  `${localDate(d)}_${pad(d.getHours())}-${pad(d.getMinutes())}-${pad(d.getSeconds())}-${pad(d.getMilliseconds(), 3)}`;

export const toLF = (s) => String(s ?? '').replace(/\r\n?/g, '\n');
export const oneLine = (s) => toLF(s).replace(/\s*\n\s*/g, ' ').trim();
/** Short content hash used for optimistic concurrency (base hashes) and idempotency keys. */
export const hashText = (s, len = 16) => crypto.createHash('sha256').update(toLF(s), 'utf8').digest('hex').slice(0, len);
/** Lines that are only an HTML comment (curator batch markers) are hidden from summaries. */
export const MARKER_LINE = /^<!--.*-->\s*$/;
export const stripMarkers = (text) => text.split('\n').filter((l) => !MARKER_LINE.test(l.trim())).join('\n');
