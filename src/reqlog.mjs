// Request log: one JSON line per HTTP request, per tool call and per curator
// run, in <log dir>/requests-YYYY-MM-DD.jsonl (src/paths.mjs).
//
// Several processes append to the same file at once (the service, stdio
// servers, the curator). Each line is written by ONE append call on a file
// opened with O_APPEND; libuv maps that to FILE_APPEND_DATA on Windows, and
// both Windows and POSIX apply such a write atomically at end of file, so lines
// never interleave. Lines are kept under 4 KB.
//
// Nothing secret is ever written: every string goes through the wiki's secret
// guard (redactSecrets) before it is truncated. Files rotate daily, and by size
// within a day (.2, .3, ...); files older than `retentionDays` are deleted.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { redactSecrets } from './secrets.mjs';
import { localDate, localISO, oneLine } from './text.mjs';

const MAX_LINE = 4000;
const NAME_RE = /^requests-(\d{4}-\d{2}-\d{2})(?:\.(\d+))?\.jsonl$/;

/** Redacts, collapses to one line and truncates a value for the log. */
export function clip(value, max = 160) {
  if (value == null) return undefined;
  const s = oneLine(redactSecrets(Array.isArray(value) ? value.join(', ') : typeof value === 'object' ? JSON.stringify(value) : String(value)));
  return s.length > max ? `${s.slice(0, max - 3)}...` : s;
}

/** Short, redacted summary of tool arguments. Bodies keep their length so a reader can tell what was sent. */
export function summarizeArgs(args, max = 160) {
  if (!args || typeof args !== 'object') return undefined;
  const out = {};
  for (const [k, v] of Object.entries(args)) {
    if (v == null || v === '') continue;
    if (k === 'body' || k === 'content') {
      out[k] = clip(v, Math.min(max, 120));
      out[`${k}_chars`] = String(v).length;
    } else out[k] = clip(v, k === 'app' ? 40 : max);
  }
  return out;
}

export const newRequestId = () => crypto.randomBytes(6).toString('hex');

/**
 * createRequestLog({dir, proc}) -> {write(entry), dir}
 * `proc` names the writer (service, stdio, curator). write() never throws.
 */
export function createRequestLog({ dir, proc, retentionDays = 30, maxBytes = 50 * 1024 * 1024, onError } = {}) {
  let failures = 0;
  let part = 1;
  let partDate = '';
  let checkedAt = 0;
  let sweptDay = '';

  const fileFor = (date, n) => path.join(dir, n > 1 ? `requests-${date}.${n}.jsonl` : `requests-${date}.jsonl`);

  function currentFile() {
    const date = localDate();
    if (date !== partDate) {
      partDate = date;
      part = 1;
      checkedAt = 0;
      // Join the highest existing part for today.
      try {
        for (const n of fs.readdirSync(dir)) {
          const m = n.match(NAME_RE);
          if (m && m[1] === date && Number(m[2] || 1) > part) part = Number(m[2]);
        }
      } catch {
        // dir missing: created on first write
      }
    }
    if (Date.now() - checkedAt > 10_000) {
      checkedAt = Date.now();
      try {
        if (fs.statSync(fileFor(date, part)).size >= maxBytes) part++;
      } catch {
        // not created yet
      }
    }
    if (sweptDay !== date) {
      sweptDay = date;
      sweep(dir, retentionDays);
    }
    return fileFor(date, part);
  }

  function write(entry) {
    if (failures > 20) return;
    const now = new Date();
    const t = localISO(now).replace(/([+-]\d\d:\d\d)$/, `.${String(now.getMilliseconds()).padStart(3, '0')}$1`);
    const rec = { t, proc, pid: process.pid, ...entry };
    for (const k of Object.keys(rec)) if (rec[k] === undefined) delete rec[k];
    let line = JSON.stringify(rec);
    if (line.length > MAX_LINE) {
      // Shrink the bulky parts rather than drop the line.
      for (const k of ['args', 'error', 'detail']) if (rec[k] !== undefined) rec[k] = clip(rec[k], 300);
      line = JSON.stringify(rec);
      if (line.length > MAX_LINE) line = JSON.stringify({ t: rec.t, proc, pid: process.pid, kind: rec.kind, rid: rec.rid, truncated: true });
    }
    try {
      fs.mkdirSync(dir, { recursive: true });
      fs.appendFileSync(currentFile(), `${line}\n`, { encoding: 'utf8', flag: 'a' });
      failures = 0;
    } catch (e) {
      failures++;
      onError?.(e);
    }
  }

  return { write, dir };
}

/** Deletes request logs older than `retentionDays`. */
export function sweep(dir, retentionDays = 30) {
  const cutoff = localDate(new Date(Date.now() - retentionDays * 86_400_000));
  let removed = 0;
  try {
    for (const n of fs.readdirSync(dir)) {
      const m = n.match(NAME_RE);
      if (m && m[1] < cutoff) {
        try {
          fs.rmSync(path.join(dir, n), { force: true });
          removed++;
        } catch {
          // in use: next sweep
        }
      }
    }
  } catch {
    // no logs yet
  }
  return removed;
}

/** A no-op logger for when logging is impossible (keeps call sites simple). */
export const nullRequestLog = { write() {}, dir: null };
