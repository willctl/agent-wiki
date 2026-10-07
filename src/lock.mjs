// Cross-process locks for the wiki folder.
//
// The mutex is still `mkdir .locks/<name>.lock` (atomic everywhere, and the same
// lock v1.1 processes take, so old and new servers exclude each other during an
// upgrade). What changed is ownership:
//
//  - The holder writes `<name>.lock/owner` as JSON: pid, process start time,
//    host, a random token, and when it took the lock.
//  - Every process that takes a lock also holds `.locks/alive/<pid>-<start>.alive`
//    open for its whole life with share mode 0 (libuv UV_FS_O_EXLOCK, Windows).
//    Windows closes that handle the moment the process dies, however it dies.
//    Anyone (any account) can test whether an owner is alive by trying to open
//    its alive file: EBUSY means alive, anything else means dead.
//
// So a lock left by a crashed or killed writer is broken immediately instead of
// after a 30 s timeout. A live owner is never broken unless it has held the lock
// for longer than `maxHoldMs` (5 min by default: a write never takes that long).
// Legacy v1.1 owners ("pid N at <time>") keep the old 30 s rule unless their pid
// is gone.
//
// Elsewhere (Linux, macOS) there is no such handle, so an owner is checked by pid,
// and its recorded start time is compared with the start time of the process that
// has that pid now (/proc on Linux, ps on macOS): a pid reused by another process
// after the owner died reads as dead, not alive. Owners are matched to this machine
// by a stable machine id (/etc/machine-id, the macOS platform UUID), since a Mac's
// hostname changes with the network.

import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const UV_FS_O_EXLOCK = 0x10000000; // libuv: open with share mode 0 (Windows only)
const IS_WIN = process.platform === 'win32';
export const PROCESS_START = Math.round(Date.now() - process.uptime() * 1000);
const HOST = os.hostname();
const START_TOLERANCE_MS = 2_000; // PROCESS_START is computed from uptime; the OS records the exec time

let machine;
/** A stable id for this machine: /etc/machine-id (Linux), the platform UUID (macOS), else the hostname. */
export function machineId() {
  if (machine !== undefined) return machine;
  machine = HOST;
  try {
    if (process.platform === 'linux') {
      for (const f of ['/etc/machine-id', '/var/lib/dbus/machine-id']) {
        const id = fs.existsSync(f) ? fs.readFileSync(f, 'utf8').trim() : '';
        if (id) {
          machine = id;
          break;
        }
      }
    } else if (process.platform === 'darwin') {
      const r = spawnSync('/usr/sbin/ioreg', ['-rd1', '-c', 'IOPlatformExpertDevice'], { encoding: 'utf8', timeout: 5000 });
      const m = (r.stdout || '').match(/"IOPlatformUUID"\s*=\s*"([^"]+)"/);
      if (m) machine = m[1];
    }
  } catch {
    // keep the hostname
  }
  return machine;
}

/**
 * When the process with this pid started (ms since the epoch), or null where that cannot be read.
 * Linux: /proc/<pid>/stat (clock ticks since boot) plus the boot time; macOS and other POSIX: ps.
 */
export function processStartMs(pid) {
  if (process.platform === 'win32' || !Number.isInteger(pid) || pid <= 0) return null;
  try {
    if (process.platform === 'linux') {
      const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
      const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' '); // field 3 onwards; the name may hold spaces
      const ticks = Number(fields[19]); // field 22: starttime
      const btime = Number(fs.readFileSync('/proc/stat', 'utf8').match(/^btime (\d+)$/m)?.[1]);
      return Number.isFinite(ticks) && btime ? btime * 1000 + Math.round((ticks * 1000) / 100) : null; // USER_HZ is 100
    }
    const r = spawnSync('ps', ['-o', 'lstart=', '-p', String(pid)], { encoding: 'utf8', timeout: 5000, env: { ...process.env, LC_ALL: 'C' } });
    const ms = Date.parse((r.stdout || '').trim());
    return Number.isFinite(ms) ? ms : null;
  } catch {
    return null; // gone between the checks, or no /proc
  }
}
const LEGACY_STALE_MS = 30_000;
const NO_OWNER_GRACE_MS = 5_000;

export class LockBusyError extends Error {}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const aliveName = (pid, start) => `${pid}-${start}.alive`;

/** fds of our own alive files, one per .locks folder. Held until the process exits. */
const aliveFds = new Map();

/** Opens (once per .locks folder) and keeps our alive file. Returns false if that is impossible here. */
function ensureAlive(locks) {
  if (!IS_WIN) return false;
  if (aliveFds.has(locks)) return aliveFds.get(locks) !== null;
  try {
    const dir = path.join(locks, 'alive');
    fs.mkdirSync(dir, { recursive: true });
    aliveFds.set(locks, fs.openSync(path.join(dir, aliveName(process.pid, PROCESS_START)), fs.constants.O_RDWR | fs.constants.O_CREAT | UV_FS_O_EXLOCK));
  } catch {
    aliveFds.set(locks, null);
  }
  return aliveFds.get(locks) !== null;
}

/**
 * Closes our alive files (tests, before deleting a wiki folder: Windows cannot
 * delete a file held with share mode 0). Only call it holding no locks.
 */
export function releaseAlive() {
  for (const [locks, fd] of aliveFds) {
    if (fd !== null) fs.closeSync(fd);
    aliveFds.delete(locks);
  }
}

/**
 * 'alive', 'dead' or 'unknown' for a lock owner record.
 * Owners that hold an alive file are checked through it; others by pid only.
 */
export function ownerLiveness(locks, owner) {
  if (!owner || !Number.isInteger(owner.pid)) return 'unknown';
  if (owner.pid === process.pid && (owner.start == null || owner.start === PROCESS_START)) return 'alive';
  if (owner.machine ? owner.machine !== machineId() : owner.host && owner.host !== HOST) return 'unknown';
  if (IS_WIN && owner.alive && owner.start != null) {
    const file = path.join(locks, 'alive', aliveName(owner.pid, owner.start));
    let fd;
    try {
      fd = fs.openSync(file, fs.constants.O_RDWR | UV_FS_O_EXLOCK);
    } catch (e) {
      if (e.code === 'EBUSY') return 'alive';
      if (e.code === 'ENOENT') return 'dead';
      return 'unknown';
    }
    fs.closeSync(fd);
    try {
      fs.unlinkSync(file);
    } catch {
      // someone else cleaned it up
    }
    return 'dead';
  }
  try {
    process.kill(owner.pid, 0);
  } catch (e) {
    if (e.code === 'ESRCH') return 'dead';
  }
  // The pid is in use. By the owner, or by a process that got its pid after it died?
  if (!IS_WIN && owner.start != null) {
    const started = processStartMs(owner.pid);
    if (started !== null && Math.abs(started - owner.start) > START_TOLERANCE_MS) return 'dead';
  }
  return 'alive';
}

/** Parses `owner`: v2 JSON, or the v1.1 text "pid N at <time>". */
export function parseOwner(text) {
  const t = String(text ?? '').trim();
  if (!t) return null;
  if (t.startsWith('{')) {
    try {
      const o = JSON.parse(t);
      return o && Number.isInteger(o.pid) ? { ...o, legacy: false } : null;
    } catch {
      return null;
    }
  }
  const m = t.match(/^pid (\d+)/);
  return m ? { pid: Number(m[1]), legacy: true } : null;
}

async function inspect(locks, lockDir) {
  let st;
  try {
    st = await fsp.stat(lockDir);
  } catch {
    return { state: 'gone' };
  }
  const owner = parseOwner(await fsp.readFile(path.join(lockDir, 'owner'), 'utf8').catch(() => ''));
  const ageMs = Date.now() - (owner?.since ?? st.mtimeMs);
  return { state: owner ? ownerLiveness(locks, owner) : 'unknown', owner, ageMs };
}

/** Moves a lock we judged stale out of the way, then deletes it. Puts it back if it turned out to be someone else's. */
async function breakLock(lockDir, expectedToken) {
  const stale = `${lockDir}.stale-${process.pid}-${crypto.randomBytes(3).toString('hex')}`;
  try {
    await fsp.rename(lockDir, stale);
  } catch {
    return false;
  }
  const owner = parseOwner(await fsp.readFile(path.join(stale, 'owner'), 'utf8').catch(() => ''));
  if (expectedToken && owner?.token && owner.token !== expectedToken) {
    await fsp.rename(stale, lockDir).catch(() => {});
    return false;
  }
  await fsp.rm(stale, { recursive: true, force: true }).catch(() => {});
  return true;
}

/**
 * Takes the named lock and returns `{ release, owner }`. Throws LockBusyError
 * after `timeoutMs` (0 = try once). `label` is recorded in the owner file.
 */
export async function acquireLock(wikiDir, name = 'write', { timeoutMs = 10_000, maxHoldMs = 5 * 60_000, label = 'agent-wiki', onBreak } = {}) {
  const locks = path.join(wikiDir, '.locks');
  await fsp.mkdir(locks, { recursive: true });
  const alive = ensureAlive(locks);
  const lockDir = path.join(locks, `${name}.lock`);
  const owner = { v: 2, pid: process.pid, start: PROCESS_START, host: HOST, machine: machineId(), alive, label, token: crypto.randomBytes(8).toString('hex') };
  const t0 = Date.now();
  for (;;) {
    let failed;
    try {
      await fsp.mkdir(lockDir);
      owner.since = Date.now();
      await fsp.writeFile(path.join(lockDir, 'owner'), `${JSON.stringify(owner)}\n`);
      break;
    } catch (e) {
      // EPERM/EACCES: on Windows a just-deleted directory can be "delete pending".
      if (!['EEXIST', 'EPERM', 'EACCES', 'EBUSY'].includes(e.code)) throw e;
      failed = e;
    }
    const seen = await inspect(locks, lockDir);
    if (seen.state === 'gone') {
      if (failed.code === 'EEXIST') continue; // released just now: take it
      // Not there, yet mkdir failed: delete pending (Windows), or no permission on .locks. Back off; give up in time.
      if (Date.now() - t0 >= timeoutMs) throw failed;
      await sleep(10 + Math.random() * 40);
      continue;
    }
    // dead: break now. No owner file yet: the owner writes it right after mkdir, so 5 s means it died in between.
    // Owner we cannot check, or a v1.1 owner: the old 30 s rule. Live owner: only after maxHoldMs.
    const stale =
      seen.state === 'dead' ||
      (seen.state === 'unknown' && seen.ageMs > (seen.owner ? LEGACY_STALE_MS : NO_OWNER_GRACE_MS)) ||
      (seen.state === 'alive' && seen.ageMs > (seen.owner?.legacy ? LEGACY_STALE_MS : maxHoldMs));
    if (stale) {
      if (await breakLock(lockDir, seen.owner?.token)) onBreak?.({ name, owner: seen.owner, state: seen.state, ageMs: seen.ageMs });
      continue;
    }
    if (Date.now() - t0 >= timeoutMs) {
      const who = seen.owner ? `${seen.owner.label || 'pid'} ${seen.owner.pid}` : 'another process';
      throw new LockBusyError(`The wiki ${name} lock is held by ${who} (for ${Math.round(seen.ageMs / 1000)}s).`);
    }
    await sleep(5 + Math.random() * 35);
  }
  let released = false;
  const release = async () => {
    if (released) return;
    released = true;
    // Rename first so the lock disappears in one step even if we die halfway through deleting it.
    const gone = `${lockDir}.released-${process.pid}-${crypto.randomBytes(3).toString('hex')}`;
    const cur = parseOwner(await fsp.readFile(path.join(lockDir, 'owner'), 'utf8').catch(() => ''));
    if (cur?.token !== owner.token) return; // broken by someone else (we held it far too long)
    try {
      await fsp.rename(lockDir, gone);
      await fsp.rm(gone, { recursive: true, force: true });
    } catch {
      await fsp.rm(lockDir, { recursive: true, force: true }).catch(() => {});
    }
  };
  return { release, owner };
}

/** Runs fn while holding the lock. */
export async function withLock(wikiDir, fn, opts = {}) {
  const { name = 'write', ...rest } = opts;
  const { release } = await acquireLock(wikiDir, name, rest);
  try {
    return await fn();
  } finally {
    await release();
  }
}

/** Who holds a lock right now, for /status and the tray: null or {owner, state, ageMs}. */
export async function lockInfo(wikiDir, name = 'write') {
  const locks = path.join(wikiDir, '.locks');
  const seen = await inspect(locks, path.join(locks, `${name}.lock`));
  return seen.state === 'gone' ? null : seen;
}

/**
 * Startup cleanup: alive files of dead processes, and leftovers of broken or
 * released locks. Never touches a live lock.
 */
export async function cleanupLocks(wikiDir) {
  const locks = path.join(wikiDir, '.locks');
  let removed = 0;
  for (const n of await fsp.readdir(locks).catch(() => [])) {
    if (/\.lock\.(?:stale|released)-/.test(n)) {
      await fsp.rm(path.join(locks, n), { recursive: true, force: true }).catch(() => {});
      removed++;
    }
  }
  if (IS_WIN) {
    for (const n of await fsp.readdir(path.join(locks, 'alive')).catch(() => [])) {
      const m = n.match(/^(\d+)-(\d+)\.alive$/);
      if (m && ownerLiveness(locks, { pid: Number(m[1]), start: Number(m[2]), host: HOST, machine: machineId(), alive: true }) === 'dead') removed++;
    }
  }
  return removed;
}
