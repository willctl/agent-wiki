// Footprint and speed of what users run: the MCP server (stdio and HTTP) and the SessionStart hook,
// the Node runtime against the Rust program, on the synthetic eval wiki.
//
//   npm run bench                              Node and Rust, alternating (Node only if Rust is not built)
//   AGENT_WIKI_IMPL=node|rust npm run bench    one side only (or --impl node|rust|both)
//   options: --runs 5   --calls 400   --concurrency 8   --json
//   AGENT_WIKI_RUST_BIN                        the Rust program (default rust/target/release/agent-wiki)
//
// Method (docs/rust-plan.md reports its numbers):
// - A run of a side measures everything once, on a fresh copy of the wiki and a fresh home. Runs
//   alternate sides (Node, Rust, Node, Rust, ...) after one discarded warm-up run per side, so file
//   caches, first-launch scans and drift do not favor one side. Each metric is the median and the
//   min-max range across runs. With 5 runs per side, ranges that do not overlap are the most extreme
//   rank-sum outcome (two-sided p about 0.008); overlapping ranges are no measurable difference.
// - Every answer is checked, not just received: no JSON-RPC, HTTP or tool error, and the text each
//   call must return (the search finds the page, the read returns the file, the note is saved, the
//   hook lists the pages rather than its could-not-read fallback). Every wiki_log note is distinct,
//   so each is a durable write rather than a duplicate the server skips, and after each run the notes
//   must be in inbox/. Errors are counted per metric and listed; any error makes the bench exit 1.
// - "process start" is the floor neither side can beat: `node -e 0`, or `agent-wiki --version`.
// - Memory is the resident set (working set on Windows) once the server has answered the calls.
// - Both sides are what ships: dist/runtime (npm run build) and a release build of the Rust program.
//   The Rust program has features the Node runtime does not (section-level search, docs/roadmap.md),
//   so this compares the two programs, not one algorithm in two languages.

import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('..', import.meta.url));
const RUNTIME = path.join(REPO, 'dist', 'runtime');
const argv = process.argv.slice(2);
const flag = (name) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : undefined;
};
function fail(msg) {
  console.error(`bench: ${msg}`);
  process.exit(2);
}
const whole = (name, d) => {
  const v = flag(name);
  if (v === undefined) return d;
  const n = Number(v);
  if (!Number.isInteger(n) || n < 1) fail(`--${name} needs a whole number of at least 1, not ${v}`);
  return n;
};
const RUNS = whole('runs', 5);
const CALLS = whole('calls', 400);
const CONCURRENCY = whole('concurrency', 8);
const STDIO_CALLS = Math.min(CALLS, 200);
const LAUNCHES = 5; // hook and process-start launches per run; the run's value is their median
const TIMEOUT_MS = 30_000;
const JSON_OUT = argv.includes('--json');

const sorted = (xs) => [...xs].sort((a, b) => a - b);
const pct = (xs, p) => (xs.length ? sorted(xs)[Math.min(xs.length - 1, Math.floor(xs.length * p))] : undefined);
function median(xs) {
  const s = sorted(xs);
  const h = Math.floor(s.length / 2);
  return s.length % 2 ? s[h] : (s[h - 1] + s[h]) / 2;
}
const mb = (bytes) => bytes / 1024 / 1024;
const fmt = (v) => (v >= 100 ? String(Math.round(v)) : v >= 10 ? v.toFixed(1) : v.toFixed(2));

/** Resident memory of a process, in bytes. */
function rss(pid) {
  if (process.platform === 'linux') {
    const m = fs.readFileSync(`/proc/${pid}/status`, 'utf8').match(/^VmRSS:\s+(\d+) kB/m);
    return Number(m[1]) * 1024;
  }
  if (process.platform === 'darwin') return Number(execFileSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' }).trim()) * 1024;
  const out = execFileSync('tasklist', ['/fi', `PID eq ${pid}`, '/fo', 'csv', '/nh'], { encoding: 'utf8' });
  const kb = Number(out.split('","')[4]?.replace(/[^\d]/g, ''));
  if (!kb) throw new Error(`no process ${pid}: ${out.trim().slice(0, 120)}`);
  return kb * 1024;
}

function dirSize(p) {
  const st = fs.statSync(p);
  if (!st.isDirectory()) return st.size;
  return fs.readdirSync(p).reduce((n, f) => n + dirSize(path.join(p, f)), 0);
}

// ------------------------------------------------------------------ the two sides

const EXE = process.platform === 'win32' ? 'agent-wiki.exe' : 'agent-wiki';
const BIN = process.env.AGENT_WIKI_RUST_BIN || path.join(REPO, 'rust', 'target', 'release', EXE);
const wanted = flag('impl') ?? process.env.AGENT_WIKI_IMPL ?? 'both';
if (!['node', 'rust', 'both'].includes(wanted)) fail(`--impl (or AGENT_WIKI_IMPL) is node, rust or both, not ${wanted}`);
let SIDES = wanted === 'both' ? ['node', 'rust'] : [wanted];
if (SIDES.includes('rust') && !fs.existsSync(BIN)) {
  const debug = fs.existsSync(path.join(REPO, 'rust', 'target', 'debug', EXE)) ? ' (a debug build is not what users run)' : '';
  const msg = `the Rust program is not built: cargo build --release in rust/, or set AGENT_WIKI_RUST_BIN${debug}`;
  if (wanted === 'rust') fail(msg);
  console.error(`bench: ${msg}; measuring Node only`);
  SIDES = ['node'];
}
if (SIDES.includes('node') && !fs.existsSync(path.join(RUNTIME, 'server.mjs'))) fail('dist/runtime is missing: npm run build');
const DEBUG_BUILD = SIDES.includes('rust') && /[\\/]debug[\\/]/.test(BIN);

const PROGRAMS = {
  node: {
    label: 'Node',
    serve: (...a) => [process.execPath, [path.join(RUNTIME, 'server.mjs'), ...a]],
    hook: [process.execPath, [path.join(RUNTIME, 'session-start.mjs')]],
    start: [process.execPath, ['-e', '0']],
    startCheck: (out) => (out === '' ? null : `node -e 0 printed ${out.slice(0, 80)}`),
    installBytes: () => fs.statSync(process.execPath).size + dirSize(RUNTIME),
    env: {},
  },
  rust: {
    label: 'Rust',
    serve: (...a) => [BIN, ['serve', ...a]],
    hook: [BIN, ['hook']],
    start: [BIN, ['--version']],
    startCheck: (out) => (/^\d+\.\d+\.\d+/.test(out.trim()) ? null : `--version printed ${out.slice(0, 80)}`),
    installBytes: () => fs.statSync(BIN).size,
    // The Rust server finds the window's files next to itself; here they are in dist/runtime/ui (as in test/impl.mjs).
    env: { AGENT_WIKI_UI_DIR: process.env.AGENT_WIKI_UI_DIR || path.join(RUNTIME, 'ui') },
  },
};

/** A fresh copy of the synthetic wiki and a fresh home, so no run sees another run's notes. */
function freshWiki(side) {
  const tmp = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'agent-wiki-bench-')));
  const wikiDir = path.join(tmp, 'wiki');
  fs.cpSync(path.join(REPO, 'eval', 'synthetic', 'wiki'), wikiDir, { recursive: true });
  const home = path.join(tmp, 'home');
  fs.mkdirSync(home);
  fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ writeMode: 'curated' }));
  const env = { ...process.env, ...PROGRAMS[side].env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: home, CLAUDE_PROJECT_DIR: tmp };
  delete env.AGENT_WIKI_WRITE_MODE;
  return { tmp, wikiDir, env };
}
const inboxNotes = (wikiDir) => fs.readdirSync(path.join(wikiDir, 'inbox')).filter((f) => f.endsWith('.md')).length;

// ------------------------------------------------------------------ calls and their checks

const CALL_MIX = [
  {
    name: 'wiki_start',
    args: () => ({ app: 'bench', topic: 'postgres upgrade' }),
    ok: (t) => t.startsWith('# Agent Wiki: session start') && t.includes('# Agent Wiki protocol'),
  },
  {
    name: 'wiki_search',
    args: () => ({ query: 'harbor deploy' }),
    ok: (t) => /^[1-9]\d* result\(s\) for "harbor deploy"/.test(t) && t.includes('pages/harbor.md'),
  },
  { name: 'wiki_search', args: () => ({ query: 'vpn dns' }), ok: (t) => /^[1-9]\d* result\(s\)/.test(t) && t.includes('pages/vpn-setup.md') },
  { name: 'wiki_read', args: () => ({ target: 'harbor' }), ok: (t) => t.startsWith('File: pages/harbor.md') && t.includes('title: Harbor') },
  { name: 'wiki_read', args: () => ({ target: 'index.md' }), ok: (t) => t.startsWith('File: index.md') && t.includes('[[harbor|Harbor]]') },
  // Distinct per call: the server answers a repeated note with "Already saved" and writes nothing.
  {
    name: 'wiki_log',
    args: (tag) => ({ app: 'bench', title: `Bench note ${tag}`, body: `a note from the benchmark (${tag})` }),
    ok: (t) => t.startsWith('Saved note '),
  },
];
const callMsg = (id, call, tag) => ({ jsonrpc: '2.0', id, method: 'tools/call', params: { name: call.name, arguments: call.args(tag) } });
const INIT = { jsonrpc: '2.0', id: 0, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'bench', version: '1' } } };
const initOk = (m) => m?.result?.serverInfo?.name === 'agent-wiki';

/** What is wrong with a tools/call answer, or null. */
function problem(call, m) {
  if (!m) return 'no answer';
  if (m.error) return `JSON-RPC error ${JSON.stringify(m.error).slice(0, 200)}`;
  const text = m.result?.content?.[0]?.text;
  if (m.result?.isError) return `tool error: ${String(text).slice(0, 200)}`;
  if (typeof text !== 'string' || !text.trim()) return 'empty result';
  if (!call.ok(text)) return `unexpected answer: ${text.slice(0, 160).replace(/\s+/g, ' ')}`;
  return null;
}

// The hook's fallback ("installed but could not be read just now") also names Agent Wiki, and it is
// what a failed or timed-out read prints: only the page list shows the hook read the wiki.
const PAGES = fs.readdirSync(path.join(REPO, 'eval', 'synthetic', 'wiki', 'pages')).filter((f) => f.endsWith('.md')).length;
const hookCheck = (out) => {
  try {
    const h = JSON.parse(out).hookSpecificOutput;
    const ctx = h?.additionalContext ?? '';
    return h?.hookEventName === 'SessionStart' && ctx.includes(`Pages (${PAGES}): `) && ctx.includes('harbor')
      ? null
      : `unexpected hook output: ${ctx.slice(0, 160)}`;
  } catch {
    return `hook output is not JSON: ${out.slice(0, 160)}`;
  }
};

/** Errors (by phase) and the work that was checked. */
class Tally {
  errors = [];
  answers = 0;
  notes = 0;
  err(phase, msg) {
    this.errors.push({ phase, msg });
  }
}

/** Waits for a process to exit; kills it after `ms`. Resolves true if it exited by itself. */
const exited = (p, ms = 10_000) =>
  new Promise((resolve) => {
    if (p.exitCode !== null || p.signalCode !== null) return resolve(true);
    const t = setTimeout(() => {
      p.kill();
      resolve(false);
    }, ms);
    p.once('exit', () => {
      clearTimeout(t);
      resolve(true);
    });
  });

// ------------------------------------------------------------------ measurements

/** `n` launches of a short-lived program; the median of the ones whose output checks out. */
/** n timed launches. Only a program that reads stdin gets `input`: writing to one that exits without
 * reading it (--version) can fail with a broken pipe although the program ran fine. */
function launches([cmd, args], env, n, check, tally, phase, input = null) {
  const times = [];
  for (let i = 0; i < n; i++) {
    const t0 = performance.now();
    let out;
    try {
      out = execFileSync(cmd, args, { env, ...(input == null ? {} : { input }), encoding: 'utf8', timeout: TIMEOUT_MS, stdio: [input == null ? 'ignore' : 'pipe', 'pipe', 'pipe'] });
    } catch (e) {
      tally.err(phase, `${e.status != null ? `exit ${e.status}` : e.message} ${String(e.stderr ?? '').slice(0, 200)}`);
      continue;
    }
    const ms = performance.now() - t0;
    const bad = check(out);
    if (bad) tally.err(phase, bad);
    else {
      tally.answers++;
      times.push(ms);
    }
  }
  return times.length ? median(times) : undefined;
}

/** A stdio server: cold start (spawn to initialize answer), per-call latency, memory after the calls. */
async function stdioRun(prog, env, calls, tally, tag) {
  const [cmd, args] = prog.serve();
  const t0 = performance.now();
  const p = spawn(cmd, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
  const waiting = new Map();
  let buf = '';
  let stderr = '';
  p.stderr.setEncoding('utf8');
  p.stderr.on('data', (d) => (stderr = (stderr + d).slice(-2000)));
  p.stdin.on('error', () => {}); // a server that died shows up as missing answers
  p.on('exit', () => {
    for (const r of waiting.values()) r(null);
    waiting.clear();
  });
  p.stdout.setEncoding('utf8');
  p.stdout.on('data', (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (!line.trim()) continue;
      let m;
      try {
        m = JSON.parse(line);
      } catch {
        tally.err('stdio', `not JSON on stdout: ${line.slice(0, 120)}`);
        continue;
      }
      const r = waiting.get(m.id);
      waiting.delete(m.id);
      r?.(m);
    }
  });
  const send = (msg) =>
    new Promise((resolve) => {
      const timer = setTimeout(() => {
        waiting.delete(msg.id);
        resolve(null);
      }, TIMEOUT_MS);
      waiting.set(msg.id, (m) => {
        clearTimeout(timer);
        resolve(m);
      });
      p.stdin.write(`${JSON.stringify(msg)}\n`);
    });

  const init = await send(INIT);
  const coldMs = performance.now() - t0;
  if (!initOk(init)) {
    tally.err('stdio', `initialize: ${init ? JSON.stringify(init).slice(0, 200) : 'no answer'} ${stderr}`);
    p.kill();
    return null;
  }
  tally.answers++;
  p.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
  const lat = [];
  let saved = 0;
  for (let i = 0; i < calls; i++) {
    const call = CALL_MIX[i % CALL_MIX.length];
    const s = performance.now();
    const r = await send(callMsg(i + 1, call, `${tag} stdio ${i}`));
    const ms = performance.now() - s;
    const bad = problem(call, r);
    if (bad) {
      tally.err('stdio', `${call.name}: ${bad}${r ? '' : ` ${stderr}`}`);
      if (!r) break; // the server is gone or stuck
      continue;
    }
    tally.answers++;
    if (call.name === 'wiki_log') saved++;
    lat.push(ms);
  }
  let mem;
  try {
    mem = rss(p.pid);
  } catch (e) {
    tally.err('stdio', `memory: ${e.message}`);
  }
  p.stdin.end();
  if (!(await exited(p))) tally.err('stdio', 'the server did not exit when its stdin closed');
  return { coldMs, lat, mem, saved };
}

/** An HTTP server: `calls` calls, `concurrency` at a time; wall time, latency, memory after, and the client's CPU. */
async function httpRun(prog, env, calls, concurrency, tally, tag) {
  const [cmd, args] = prog.serve('--http', '--port', '0', '--parent-stdin');
  const p = spawn(cmd, args, { env, stdio: ['pipe', 'ignore', 'pipe'] });
  p.stdin.on('error', () => {});
  let err = '';
  p.stderr.setEncoding('utf8');
  const url = await new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), 15_000);
    p.once('exit', () => resolve(null));
    p.stderr.on('data', (d) => {
      err = (err + d).slice(-4000);
      const m = err.match(/listening on (http:\/\/127\.0\.0\.1:\d+\/mcp)/);
      if (m) {
        clearTimeout(timer);
        resolve(m[1]);
      }
    });
  });
  if (!url) {
    tally.err('http', `the server did not start: ${err}`);
    p.kill();
    return null;
  }
  const headers = { 'content-type': 'application/json', accept: 'application/json, text/event-stream' };
  const post = async (msg) => {
    try {
      const r = await fetch(url, { method: 'POST', headers, body: JSON.stringify(msg), signal: AbortSignal.timeout(TIMEOUT_MS) });
      const body = await r.text();
      if (!r.ok) return { fail: `HTTP ${r.status} ${body.slice(0, 160)}` };
      try {
        return { m: JSON.parse(body) };
      } catch {
        return { fail: `not JSON (${r.headers.get('content-type')}): ${body.slice(0, 120)}` };
      }
    } catch (e) {
      return { fail: `${e.name}: ${e.message}` };
    }
  };
  const init = await post(INIT);
  if (init.fail || !initOk(init.m)) {
    tally.err('http', `initialize: ${init.fail ?? JSON.stringify(init.m).slice(0, 200)}`);
    p.kill();
    return null;
  }
  tally.answers++;
  const lat = [];
  let next = 0;
  let saved = 0;
  const cpu0 = process.cpuUsage();
  const t0 = performance.now();
  await Promise.all(
    Array.from({ length: concurrency }, async () => {
      while (next < calls) {
        const i = next++;
        const call = CALL_MIX[i % CALL_MIX.length];
        const s = performance.now();
        const { m, fail } = await post(callMsg(i + 1, call, `${tag} http ${i}`));
        const ms = performance.now() - s;
        const bad = fail ?? problem(call, m);
        if (bad) {
          tally.err('http', `${call.name}: ${bad}`);
          continue;
        }
        tally.answers++;
        if (call.name === 'wiki_log') saved++;
        lat.push(ms);
      }
    }),
  );
  const wallMs = performance.now() - t0;
  const cpu = process.cpuUsage(cpu0);
  let mem;
  try {
    mem = rss(p.pid);
  } catch (e) {
    tally.err('http', `memory: ${e.message}`);
  }
  p.stdin.end();
  if (!(await exited(p))) tally.err('http', 'the server did not exit when its parent stdin closed');
  // CPU time is counted in ticks (about 16 ms on Windows): too coarse for a phase shorter than 250 ms.
  const clientCpu = wallMs >= 250 ? ((cpu.user + cpu.system) / 1000 / wallMs) * 100 : undefined;
  return { lat, wallMs, mem, saved, clientCpu };
}

/** One run of one side: every metric once. */
async function oneRun(side, calls, tally, tag) {
  const prog = PROGRAMS[side];
  const { tmp, wikiDir, env } = freshWiki(side);
  try {
    const before = inboxNotes(wikiDir);
    const v = { install: mb(prog.installBytes()) };
    v.start = launches(prog.start, env, LAUNCHES, prog.startCheck, tally, 'start');
    v.hook = launches(prog.hook, env, LAUNCHES, hookCheck, tally, 'hook', '{}');
    const st = await stdioRun(prog, env, Math.min(calls, 200), tally, tag);
    if (st) Object.assign(v, { stdioCold: st.coldMs, stdioP50: pct(st.lat, 0.5), stdioP95: pct(st.lat, 0.95), stdioMem: st.mem && mb(st.mem) });
    const ht = await httpRun(prog, env, calls, CONCURRENCY, tally, tag);
    if (ht) {
      Object.assign(v, { httpWall: ht.wallMs, httpP50: pct(ht.lat, 0.5), httpP95: pct(ht.lat, 0.95), httpMem: ht.mem && mb(ht.mem) });
      v.httpClientCpu = ht.clientCpu;
    }
    const saved = (st?.saved ?? 0) + (ht?.saved ?? 0);
    const written = inboxNotes(wikiDir) - before;
    if (written !== saved) tally.err('notes', `${saved} notes answered "Saved" but ${written} new notes are in inbox/`);
    tally.notes += written;
    return v;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 5 });
  }
}

// ------------------------------------------------------------------ run and report

const METRICS = [
  // key, label, unit, error phases
  ['install', 'install size', 'MB', []],
  ['start', 'process start, no work', 'ms', ['start']],
  ['hook', 'session hook', 'ms', ['hook']],
  ['stdioCold', 'stdio cold start', 'ms', ['stdio']],
  ['stdioP50', `stdio call p50 (${STDIO_CALLS} calls)`, 'ms', ['stdio']],
  ['stdioP95', `stdio call p95 (${STDIO_CALLS} calls)`, 'ms', ['stdio']],
  ['stdioMem', 'stdio memory after calls', 'MB', ['stdio']],
  ['httpWall', `http ${CALLS} calls x${CONCURRENCY}, wall`, 'ms', ['http']],
  ['httpP50', 'http call p50', 'ms', ['http']],
  ['httpP95', 'http call p95', 'ms', ['http']],
  ['httpMem', 'http memory after calls', 'MB', ['http']],
  ['httpClientCpu', 'http load generator CPU (this process)', '% of a core', ['http'], { diagnostic: true }],
];

const rustVersion = SIDES.includes('rust') ? execFileSync(BIN, ['--version'], { encoding: 'utf8' }).trim() : undefined;
const load = process.platform === 'win32' ? 'n/a on Windows' : os.loadavg().map((x) => x.toFixed(2)).join(' ');
const header = [
  `bench: ${process.platform}-${process.arch}, ${os.cpus().length} CPUs, load ${load}; node ${process.version}` +
    (rustVersion ? `; Rust ${rustVersion} at ${BIN}` : ''),
  `${RUNS} run(s) per side${SIDES.length > 1 ? `, alternating ${SIDES.map((s) => PROGRAMS[s].label).join(', ')}` : ''}, ` +
    `after one warm-up run each; fresh wiki per run; hook and process start are the median of ${LAUNCHES} launches per run`,
];
if (DEBUG_BUILD) header.push('WARNING: the Rust program is a debug build, not what users run: the Rust side is untuned.');
if (RUNS < 5) header.push(`WARNING: fewer than 5 runs per side: treat every comparison as inconclusive.`);
if (!JSON_OUT) console.log(header.join('\n'));

const tallies = Object.fromEntries(SIDES.map((s) => [s, new Tally()]));
const samples = Object.fromEntries(SIDES.map((s) => [s, {}]));
for (const side of SIDES) {
  const warm = new Tally();
  await oneRun(side, CALL_MIX.length * 2, warm, 'warmup');
  for (const e of warm.errors) tallies[side].err(e.phase, `(warm-up) ${e.msg}`);
}
for (let r = 0; r < RUNS; r++) {
  for (const side of SIDES) {
    process.stderr.write(`bench: run ${r + 1}/${RUNS} ${PROGRAMS[side].label}\n`);
    const v = await oneRun(side, CALLS, tallies[side], `r${r}`);
    for (const [k, x] of Object.entries(v)) if (Number.isFinite(x)) (samples[side][k] ??= []).push(x);
  }
}

const stat = (xs) => (xs?.length ? { median: median(xs), min: Math.min(...xs), max: Math.max(...xs), n: xs.length, samples: xs } : null);
const errorsIn = (side, phases) => tallies[side].errors.filter((e) => phases.includes(e.phase)).length;
function verdict(a, b) {
  if (!a || !b) return 'no verdict: a side has no samples';
  if (Math.min(a.n, b.n) < 5) return 'inconclusive: fewer than 5 runs per side';
  const x = (p, q) => `${(p / q).toFixed(p / q >= 10 ? 0 : 1)}x`;
  if (b.max < a.min) return `Rust lower, ${x(a.median, b.median)} (ranges apart)`;
  if (b.min > a.max) return `Rust higher, ${x(b.median, a.median)} (ranges apart)`;
  return 'ranges overlap: no measurable difference';
}

const lines = [];
const result = { platform: `${process.platform}-${process.arch}`, cpus: os.cpus().length, load, node: process.version, rust: rustVersion, rustBin: rustVersion ? BIN : undefined, debugBuild: DEBUG_BUILD || undefined, runs: RUNS, calls: CALLS, stdioCalls: STDIO_CALLS, concurrency: CONCURRENCY, launchesPerRun: LAUNCHES, warnings: header.filter((h) => h.startsWith('WARNING')), sides: {} };
for (const side of SIDES) result.sides[side] = { metrics: {}, answersChecked: tallies[side].answers, notesWritten: tallies[side].notes, errors: tallies[side].errors };
for (const [key, label, unit, phases, opts] of METRICS) {
  const stats = SIDES.map((s) => stat(samples[s][key]));
  SIDES.forEach((s, i) => (result.sides[s].metrics[key] = stats[i] && { ...stats[i], unit }));
  const parts = SIDES.map((s, i) => (stats[i] ? `${PROGRAMS[s].label} ${fmt(stats[i].median)} ${unit} (range ${fmt(stats[i].min)}-${fmt(stats[i].max)})` : `${PROGRAMS[s].label} -`));
  const ns = stats.map((st) => st?.n ?? 0);
  const n = ns.every((x) => x === ns[0]) ? `n=${ns[0]}${SIDES.length > 1 ? ' per side' : ''}` : `n=${ns.join('/')}`;
  const k = SIDES.reduce((sum, s) => sum + errorsIn(s, phases), 0);
  let line = `${label}: ${parts.join(' vs ')}, ${n}, errors ${k}`;
  if (SIDES.length === 2 && !opts?.diagnostic) line += `; ${verdict(...stats)}`;
  lines.push(line);
}
lines.push(
  `checked: ${SIDES.map((s) => `${PROGRAMS[s].label} ${tallies[s].answers} answers, ${tallies[s].notes} notes written and found in inbox/`).join('; ')}` +
    `, errors ${SIDES.reduce((sum, s) => sum + errorsIn(s, ['notes']), 0)}`,
);
const errorCount = SIDES.reduce((n, s) => n + tallies[s].errors.length, 0);
for (const s of SIDES) {
  const errs = tallies[s].errors;
  if (!errs.length) continue;
  lines.push(`${PROGRAMS[s].label} errors (${errs.length}):`);
  for (const e of errs.slice(0, 10)) lines.push(`  ${e.phase}: ${e.msg.replace(/\s+/g, ' ').slice(0, 300)}`);
  if (errs.length > 10) lines.push(`  ... and ${errs.length - 10} more`);
}
if (errorCount) {
  lines.push(`FAILED: ${errorCount} error(s); the numbers above include failed or wrong answers and are not valid.`);
  process.exitCode = 1;
}
if (JSON_OUT) console.log(JSON.stringify(result));
else console.log(lines.join('\n'));
