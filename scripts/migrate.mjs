// Moves a pre-1.3 ~/.agent-wiki to the standard locations (src/paths.mjs), as part of
// `npm run install-local`, or by hand:
//
//   node scripts/migrate.mjs            plan only: what would move where
//   node scripts/migrate.mjs --run      finish: merge logs apps wrote to the old folder after the move, remove it
//   node scripts/migrate.mjs --rollback move everything this migration moved back (then install the old version)
//
// Each item is a rename, not a copy (~/.agent-wiki and %LOCALAPPDATA% are on the same volume), so the
// curator's sign-in and the window's profile arrive intact. Every step is recorded in a journal
// (<state>/migration.json) before and after it happens: a run that stops halfway resumes where it
// stopped, and --rollback undoes it. Build outputs (runtime, tray, service, marketplace, paste file)
// that already exist at the new location are deleted from the old one, since install-local writes
// them again anyway; anything else that exists on both sides is left where it is and reported. Logs
// are merged. The old folder is removed only when it is empty.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const fwd = (p) => String(p).replace(/\\/g, '/');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** What lives in the old folder and where each item goes. `rebuilt`: install-local writes it again. */
export function migrationItems(P) {
  return [
    { name: 'config.json', to: P.config },
    { name: 'install-state.json', to: P.state },
    { name: 'logs', to: P.logs, merge: true },
    { name: 'curator', to: P.curatorDir },
    { name: 'ui-profile', to: P.uiProfile },
    { name: 'runtime', to: P.runtime, rebuilt: true },
    { name: 'tray', to: P.trayDir, rebuilt: true },
    { name: 'service', to: P.serviceDir, rebuilt: true },
    { name: 'marketplace', to: P.market, rebuilt: true },
    { name: 'paste-into-app-settings.md', to: P.paste, rebuilt: true },
  ];
}

/**
 * Copies of the standard folders that an app package captured (Windows). A process inside an MSIX
 * app (Claude desktop's terminal, Store PowerShell, ChatGPT desktop) that creates files under AppData
 * writes them to that package's LocalCache instead, where nothing outside the package sees them: on
 * 2026-10-02 an install-local run from Claude desktop's Code tab moved everything there. Each copy is a
 * migration source like the old ~/.agent-wiki. Returns [{ pkg, legacy, items, journal }].
 */
export function packagedCopies(P, { localAppData = process.env.LOCALAPPDATA, appData = process.env.APPDATA } = {}) {
  if (!localAppData) return [];
  const root = path.join(localAppData, 'Packages');
  let pkgs;
  try {
    pkgs = fs.readdirSync(root);
  } catch {
    return [];
  }
  const localRel = path.relative(localAppData, P.dataDir);
  const roamingRel = appData ? path.relative(appData, P.configDir) : null;
  const out = [];
  for (const pkg of pkgs) {
    const local = path.join(root, pkg, 'LocalCache', 'Local', localRel);
    if (!localRel.startsWith('..') && exists(local)) {
      out.push({
        pkg,
        legacy: local,
        journal: path.join(P.stateDir, `migration-${pkg}-local.json`),
        items: [
          { name: 'state', to: P.stateDir, merge: true },
          { name: 'logs', to: P.logs, merge: true },
          { name: 'curator', to: P.curatorDir },
          { name: 'ui-profile', to: P.uiProfile },
          { name: 'runtime', to: P.runtime, rebuilt: true },
          { name: 'tray', to: P.trayDir, rebuilt: true },
          { name: 'service', to: P.serviceDir, rebuilt: true },
          { name: 'marketplace', to: P.market, rebuilt: true },
          { name: 'paste-into-app-settings.md', to: P.paste, rebuilt: true },
        ],
      });
    }
    const roaming = roamingRel && !roamingRel.startsWith('..') ? path.join(root, pkg, 'LocalCache', 'Roaming', roamingRel) : null;
    if (roaming && exists(roaming)) {
      out.push({ pkg, legacy: roaming, journal: path.join(P.stateDir, `migration-${pkg}-roaming.json`), items: [{ name: 'config.json', to: P.config }] });
    }
  }
  return out;
}

const exists = (p) => fs.existsSync(p);

async function readJson(file) {
  try {
    return JSON.parse((await fsp.readFile(file, 'utf8')).replace(/^﻿/, ''));
  } catch (e) {
    if (e.code === 'ENOENT') return null;
    throw e;
  }
}

async function writeJson(file, value) {
  await fsp.mkdir(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  await fsp.writeFile(tmp, `${JSON.stringify(value, null, 2)}\n`);
  await fsp.rename(tmp, file);
}

/** fs.rename, retried while Windows reports the source busy (an indexer, antivirus, a process just exiting). */
async function renameRetry(from, to, { tries = 20, onBusy } = {}) {
  for (let i = 0; ; i++) {
    try {
      await fsp.rename(from, to);
      return;
    } catch (e) {
      if (i >= tries || !['EPERM', 'EACCES', 'EBUSY'].includes(e.code)) {
        const err = new Error(`could not move ${fwd(from)} to ${fwd(to)}: ${e.code || e.message}${onBusy ? `\n${onBusy(from)}` : ''}`);
        err.code = e.code;
        throw err;
      }
      await sleep(100 + i * 100);
    }
  }
}

async function sameFile(a, b) {
  const [x, y] = await Promise.all([fsp.readFile(a), fsp.readFile(b)]);
  return x.equals(y);
}

/**
 * Moves every entry of `from` into `to`. A file on both sides: identical, the old one is dropped;
 * a log (.jsonl, .log, rotated .1) is appended to the new one in place (a running service may be
 * writing it: no rewrite, no rename); http-sessions.json keeps the new one (it is a cache). Returns
 * what it did.
 */
export async function mergeDir(from, to) {
  await fsp.mkdir(to, { recursive: true });
  const done = [];
  for (const e of await fsp.readdir(from, { withFileTypes: true })) {
    const a = path.join(from, e.name);
    const b = path.join(to, e.name);
    if (!exists(b)) {
      await renameRetry(a, b);
      done.push(`${e.name}: moved`);
    } else if (e.isDirectory()) {
      done.push(...(await mergeDir(a, b)).map((d) => `${e.name}/${d}`));
      await fsp.rmdir(a);
    } else if (await sameFile(a, b)) {
      await fsp.rm(a);
      done.push(`${e.name}: identical`);
    } else if (/\.(jsonl|log)(\.\d+)?$/.test(e.name)) {
      const old = await fsp.readFile(a);
      const cur = await fsp.stat(b);
      const tail = cur.size ? Buffer.alloc(1) : null;
      if (tail) {
        const fh = await fsp.open(b, 'r');
        await fh.read(tail, 0, 1, cur.size - 1).finally(() => fh.close());
      }
      const sep = tail && tail[0] !== 0x0a ? '\n' : '';
      await fsp.appendFile(b, Buffer.concat([Buffer.from(sep), old]));
      await fsp.rm(a);
      done.push(`${e.name}: appended`);
    } else if (e.name === 'http-sessions.json') {
      await fsp.rm(a);
      done.push(`${e.name}: kept the new one`);
    } else {
      done.push(`${e.name}: CONFLICT, left in ${fwd(from)}`);
    }
  }
  return done;
}

/** A path under an old item, mapped to the item's new place (or back, with `reverse`). */
export function remapper(legacy, items, { reverse = false } = {}) {
  const pairs = items.map((it) => (reverse ? [it.to, path.join(legacy, it.name)] : [path.join(legacy, it.name), it.to]));
  return (p) => {
    if (typeof p !== 'string' || !path.isAbsolute(p)) return p;
    for (const [a, b] of pairs) {
      const rel = path.relative(a, p);
      if (rel === '' || (!rel.startsWith('..') && !path.isAbsolute(rel))) {
        const out = rel ? path.join(b, rel) : b;
        return p.includes('\\') ? out : fwd(out);
      }
    }
    return p;
  };
}

function remapJson(value, map) {
  if (typeof value === 'string') return map(value);
  if (Array.isArray(value)) return value.map((v) => remapJson(v, map));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, remapJson(v, map)]));
  return value;
}

/** Rewrites the old paths inside the moved config.json and install-state.json. */
async function fixUpJson(files, map) {
  const changed = [];
  for (const file of files) {
    const cur = await readJson(file).catch(() => null);
    if (!cur) continue;
    const next = remapJson(cur, map);
    if (JSON.stringify(next) !== JSON.stringify(cur)) {
      await writeJson(file, next);
      changed.push(file);
    }
  }
  return changed;
}

/** What a migration would do, without touching anything. */
export async function planMigration({ legacy, items }) {
  if (!exists(legacy)) return { needed: false, items: [], unknown: [] };
  const names = new Set(items.map((i) => i.name));
  const out = [];
  for (const it of items) {
    const from = path.join(legacy, it.name);
    if (!exists(from)) continue;
    out.push({ ...it, from, action: !exists(it.to) ? 'move' : it.merge ? 'merge' : it.rebuilt ? 'replace' : 'conflict' });
  }
  const unknown = (await fsp.readdir(legacy)).filter((n) => !names.has(n));
  return { needed: true, items: out, unknown };
}

/**
 * Runs (or resumes) the migration. `log(msg)` reports progress; `onBusy(path)` explains a source
 * that stays locked (which process holds it). Returns { moved, conflicts, unknown, removedLegacy }.
 */
export async function migrate({ legacy, items, journal, log = () => {}, onBusy, removeLegacy = true }) {
  const j = (await readJson(journal)) || { startedAt: new Date().toISOString(), legacy: fwd(legacy), items: {} };
  const save = () => writeJson(journal, j);
  // Written before anything moves: the journal's folder may be a destination (then it is merged into, not renamed onto).
  await save();
  const conflicts = [];
  for (const it of items) {
    const from = path.join(legacy, it.name);
    const rec = (j.items[it.name] ||= { from: fwd(from), to: fwd(it.to), status: 'pending' });
    if (!exists(from)) {
      // Done by an earlier run that stopped before recording it (renames are atomic), or never there.
      if (rec.status === 'moving') rec.status = 'moved';
      else if (rec.status === 'merging') rec.status = 'merged';
      else if (rec.status === 'pending') rec.status = 'absent';
      continue;
    }
    if (!exists(it.to)) {
      rec.status = 'moving';
      await save();
      await fsp.mkdir(path.dirname(it.to), { recursive: true });
      await renameRetry(from, it.to, { onBusy });
      rec.status = 'moved';
      await save();
      log(`moved ${it.name} -> ${fwd(it.to)}`);
    } else if (it.merge) {
      rec.status = 'merging';
      await save();
      const done = await mergeDir(from, it.to);
      const left = done.filter((d) => d.includes('CONFLICT'));
      if (!left.length) await fsp.rmdir(from).catch(() => {});
      rec.status = left.length ? 'conflict' : 'merged';
      rec.detail = done;
      await save();
      log(`merged ${it.name} into ${fwd(it.to)}${left.length ? ` (${left.length} left behind)` : ''}`);
      if (left.length) conflicts.push(...left.map((d) => `${it.name}/${d}`));
    } else if (it.rebuilt) {
      await fsp.rm(from, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
      rec.status = 'replaced';
      await save();
      log(`removed the old ${it.name} (a newer one is at ${fwd(it.to)})`);
    } else if (!fs.statSync(from).isDirectory() && (await sameFile(from, it.to))) {
      await fsp.rm(from);
      rec.status = 'moved';
      await save();
    } else {
      rec.status = 'conflict';
      await save();
      conflicts.push(`${it.name}: exists at both ${fwd(from)} and ${fwd(it.to)}; left in place`);
    }
  }
  const map = remapper(legacy, items);
  const configItem = items.find((i) => i.name === 'config.json');
  const stateItem = items.find((i) => i.name === 'install-state.json');
  const fixed = await fixUpJson([configItem?.to, stateItem?.to].filter(Boolean), map);
  for (const f of fixed) log(`rewrote old paths in ${fwd(f)}`);
  const names = new Set(items.map((i) => i.name));
  const unknown = exists(legacy) ? (await fsp.readdir(legacy)).filter((n) => !names.has(n)) : [];
  let removedLegacy = !exists(legacy);
  if (removeLegacy && exists(legacy) && !(await fsp.readdir(legacy)).length) {
    await fsp.rmdir(legacy);
    removedLegacy = true;
    log(`removed the empty ${fwd(legacy)}`);
  }
  j.finishedAt = new Date().toISOString();
  j.removedLegacy = removedLegacy;
  j.conflicts = conflicts;
  j.unknown = unknown;
  await save();
  return { journal: j, conflicts, unknown, removedLegacy };
}

/** Moves back what the journal says was moved; merged logs stay where they are. */
export async function rollback({ legacy, items, journal, log = () => {} }) {
  const j = await readJson(journal);
  if (!j) throw new Error(`no migration journal at ${fwd(journal)}`);
  await fsp.mkdir(legacy, { recursive: true });
  const back = items.filter((it) => ['moved', 'moving'].includes(j.items[it.name]?.status));
  // Paths inside the files first, while they are still at their new place.
  const map = remapper(legacy, items, { reverse: true });
  await fixUpJson(back.filter((it) => ['config.json', 'install-state.json'].includes(it.name)).map((it) => it.to), map);
  for (const it of back) {
    const from = path.join(legacy, it.name);
    if (exists(from) || !exists(it.to)) continue;
    await renameRetry(it.to, from);
    j.items[it.name].status = 'rolled-back';
    log(`moved ${it.name} back to ${fwd(from)}`);
  }
  j.rolledBackAt = new Date().toISOString();
  await writeJson(journal, j);
  return j;
}

// ---------------------------------------------------------------- Windows: what holds the old folder

/** Processes whose executable or command line is under `dir` (Windows; [] elsewhere). */
export function processesUnder(dir) {
  if (process.platform !== 'win32') return [];
  const ps = `$d = ${psQuote(path.resolve(dir))}; Get-CimInstance Win32_Process | Where-Object { ($_.ExecutablePath -and $_.ExecutablePath.StartsWith($d, 'OrdinalIgnoreCase')) -or ($_.CommandLine -and ($_.CommandLine.Replace('/', '\\').IndexOf($d, [StringComparison]::OrdinalIgnoreCase) -ge 0)) } | ForEach-Object { [pscustomobject]@{ pid = $_.ProcessId; ppid = $_.ParentProcessId; name = $_.Name; exe = $_.ExecutablePath; cmd = $_.CommandLine } } | ConvertTo-Json -Compress`;
  const r = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', ps], { encoding: 'utf8', windowsHide: true, timeout: 60_000 });
  const t = (r.stdout || '').trim();
  if (!t) return [];
  const v = JSON.parse(t);
  // Not this query's own PowerShell, whose command line names the folder too.
  return (Array.isArray(v) ? v : [v]).filter((p) => p.pid !== process.pid && !/Get-CimInstance Win32_Process/.test(p.cmd || ''));
}

const psQuote = (s) => `'${String(s).replace(/'/g, "''")}'`;

export function describeProcesses(list) {
  return list.map((p) => `pid ${p.pid} ${p.name}${p.cmd ? `: ${p.cmd.length > 140 ? `${p.cmd.slice(0, 137)}...` : p.cmd}` : ''}`).join('\n');
}

// ---------------------------------------------------------------- CLI

async function cli() {
  const { P } = await import('./lib.mjs');
  const items = migrationItems(P);
  const args = process.argv.slice(2);
  if (args.includes('--rollback')) {
    const j = await rollback({ legacy: P.legacyHome, items, journal: P.migration, log: (m) => console.log(`  ${m}`) });
    console.log(`Rolled back (${Object.values(j.items).filter((i) => i.status === 'rolled-back').length} items). Install the previous version to use it again.`);
    return;
  }
  const plan = await planMigration({ legacy: P.legacyHome, items });
  if (args.includes('--run')) {
    // Finishing only: what apps still running the old version wrote to the old folder after the move
    // (logs). A first migration goes through install-local, which stops what holds the files.
    const { appDataVirtualized } = await import('./lib.mjs');
    const pkg = appDataVirtualized();
    if (pkg) throw new Error(`This terminal runs inside the app package ${pkg}; run it from a normal terminal (Win+R, cmd).`);
    const more = plan.items.filter((it) => it.name !== 'logs');
    if (more.length) throw new Error(`${fwd(P.legacyHome)} holds more than logs (${more.map((i) => i.name).join(', ')}): run npm run install-local.`);
    const log = (m) => console.log(`  ${m}`);
    for (const c of packagedCopies(P)) await migrate({ legacy: c.legacy, items: c.items, journal: c.journal, log });
    const res = plan.needed ? await migrate({ legacy: P.legacyHome, items, journal: P.migration, log }) : { removedLegacy: true, conflicts: [], unknown: [] };
    console.log(res.removedLegacy ? `Done: ${fwd(P.legacyHome)} is gone.` : `${fwd(P.legacyHome)} stays: ${[...res.conflicts, ...res.unknown].join('; ')}`);
    return;
  }
  if (!plan.needed) {
    console.log(`Nothing to migrate: ${fwd(P.legacyHome)} does not exist.`);
    return;
  }
  console.log(`From ${fwd(P.legacyHome)}:`);
  for (const it of plan.items) console.log(`  ${it.action.padEnd(8)} ${it.name} -> ${fwd(it.to)}`);
  for (const n of plan.unknown) console.log(`  keep     ${n} (not Agent Wiki's; stays, and the folder with it)`);
  console.log('\n`npm run install-local` performs it (or `node scripts/migrate.mjs --run` when only logs are left).');
}

if (path.resolve(fs.realpathSync(process.argv[1] || '.')) === fs.realpathSync(fileURLToPath(import.meta.url))) {
  cli().catch((e) => {
    console.error(e?.stack || e);
    process.exit(1);
  });
}
