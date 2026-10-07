// The isolated `codex exec` both model users share: the curator (an edit plan) and Ask (an answer
// found with the wiki's read-only tools). It runs as the user, in the curator's own CODEX_HOME, with
// the user's config, rules, plugins, hooks, apps, memories, shell and web search off, and returns
// the final message parsed as JSON (--output-schema). It never reads Codex's credential files.

import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { clip } from './reqlog.mjs';

export class ModelError extends Error {
  constructor(kind, message, extra = {}) {
    super(message);
    this.kind = kind; // signed_out | rate_limited | config | timeout | model_error | bad_output | aborted
    Object.assign(this, extra);
  }
}

const FEATURES_OFF = [
  'plugins', 'apps', 'hooks', 'memories', 'multi_agent', 'goals', 'shell_tool', 'unified_exec', 'view_image',
  'image_generation', 'skill_search', 'tool_suggest', 'browser_use', 'computer_use', 'in_app_browser', 'sleep_tool',
];

/** The isolated invocation. `extra`: more arguments (Ask adds its MCP server and its reasoning effort). */
export function codexArgs(cfg, { workDir, schemaFile, outFile, extra = [] }) {
  const c = (kv) => ['-c', kv];
  return [
    'exec', '--ephemeral', '--skip-git-repo-check', '--ignore-user-config', '--ignore-rules', '--strict-config',
    '--sandbox', 'read-only', '-C', workDir, '-m', cfg.model,
    ...c(`model_reasoning_effort="${cfg.reasoningEffort}"`), ...c('model_reasoning_summary="none"'), ...c('model_verbosity="low"'),
    ...c('forced_login_method="chatgpt"'), ...c('web_search="disabled"'), ...c('project_doc_max_bytes=0'), ...c('agents.enabled=false'),
    ...c('skills.include_instructions=false'), ...c('skills.bundled.enabled=false'), ...c('include_permissions_instructions=false'),
    ...c('include_collaboration_mode_instructions=false'), ...c('include_environment_context=false'), ...c('include_apps_instructions=false'),
    ...c('history.persistence="none"'),
    ...extra,
    ...FEATURES_OFF.flatMap((f) => ['--disable', f]),
    '--output-schema', schemaFile, '-o', outFile, '--json', '-',
  ];
}

export function codexEnv(cfg) {
  const env = { ...process.env, CODEX_HOME: cfg.codexHome };
  delete env.CODEX_API_KEY;
  delete env.OPENAI_API_KEY;
  return env;
}

/**
 * How to start codex: [command, ...leading args]. A JavaScript entry point runs with this Node, which
 * includes an npm-installed `codex` on Linux and macOS: an extensionless symlink to a script with a
 * `#!/usr/bin/env node` line, which a service's minimal PATH could not run.
 */
export function codexCommand(codexPath) {
  let target = codexPath;
  try {
    target = fs.realpathSync(codexPath);
  } catch {
    // not a path (a bare "codex" on PATH): run as given
  }
  if (/\.[cm]?js$/i.test(target)) return [process.execPath, target];
  if (process.platform !== 'win32') {
    try {
      const fd = fs.openSync(target, 'r');
      const head = Buffer.alloc(128);
      const n = fs.readSync(fd, head, 0, 128, 0);
      fs.closeSync(fd);
      const line = head.subarray(0, n).toString('utf8').split('\n')[0];
      if (/^#!.*\bnode\b/.test(line)) return [process.execPath, target];
    } catch {
      // unreadable or not there: let spawn report it
    }
  }
  return [codexPath];
}

function spawnCodex(cfg, args, opts) {
  const [cmd, ...lead] = codexCommand(cfg.codexPath);
  // Its own process group on POSIX, so killTree stops codex and what it started (Ask's read-only server).
  return spawn(cmd, [...lead, ...args], { windowsHide: true, detached: process.platform !== 'win32', ...opts });
}

/** Stops a codex child and everything it started: taskkill /T on Windows, the process group elsewhere. */
export function killTree(child) {
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return;
  if (process.platform === 'win32') {
    const tk = spawn('taskkill.exe', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
    tk.on('error', () => child.kill());
    return;
  }
  try {
    process.kill(-child.pid, 'SIGTERM');
  } catch {
    child.kill();
  }
}

export function classify(text) {
  if (/not logged in|log ?in again|sign in again|could not be refreshed|unauthori[sz]ed|\b401\b|login required|no auth/i.test(text)) return 'signed_out';
  if (/usage limit|rate limit|\b429\b|quota exceeded|too many requests/i.test(text)) return 'rate_limited';
  if (/unknown configuration field|unknown feature|unexpected argument|error parsing -c|invalid value for/i.test(text)) return 'config';
  return 'model_error';
}

/** `codex login status` for the curator's CODEX_HOME. */
export function loginStatus(cfg) {
  return new Promise((resolve) => {
    let out = '';
    let child;
    try {
      child = spawnCodex(cfg, ['login', 'status'], { env: codexEnv(cfg), stdio: ['ignore', 'pipe', 'pipe'] });
    } catch (e) {
      resolve({ signedIn: false, detail: e.message });
      return;
    }
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (out += d));
    const timer = setTimeout(() => killTree(child), 30_000);
    child.on('error', (e) => resolve({ signedIn: false, detail: e.message }));
    child.on('close', (code) => {
      clearTimeout(timer);
      resolve({ signedIn: code === 0 && /logged in/i.test(out) && !/not logged in/i.test(out), detail: clip(out, 200) });
    });
  });
}

/**
 * Runs one isolated `codex exec` and returns {output (the final message as JSON), usage, ms}. Throws
 * ModelError. `schema`: the output contract; `onEvent(e)`: each JSONL event as it arrives; `signal`
 * aborts (stop requested).
 */
export async function runModel(cfg, prompt, { runDir, signal, schema, schemaName = 'plan', extra = [], onEvent, env = {} }) {
  await fsp.mkdir(runDir, { recursive: true });
  const workDir = path.join(runDir, 'work');
  await fsp.mkdir(workDir, { recursive: true });
  const schemaFile = path.join(runDir, `${schemaName}.schema.json`);
  await fsp.writeFile(schemaFile, JSON.stringify(schema));
  const outFile = path.join(runDir, `out-${process.pid}-${crypto.randomBytes(4).toString('hex')}.json`);
  const t0 = Date.now();
  const events = [];
  let stderr = '';
  const child = spawnCodex(cfg, codexArgs(cfg, { workDir, schemaFile, outFile, extra }), { env: { ...codexEnv(cfg), ...env }, stdio: ['pipe', 'pipe', 'pipe'], cwd: workDir });
  let buf = '';
  child.stdout.setEncoding('utf8');
  child.stdout.on('data', (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (!line) continue;
      let e;
      try {
        e = JSON.parse(line);
      } catch {
        continue; // not JSON: ignore
      }
      events.push(e);
      try {
        onEvent?.(e);
      } catch {
        // a progress listener must not break the run
      }
    }
  });
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (d) => {
    if (stderr.length < 64_000) stderr += d;
  });
  child.stdin.on('error', () => {});
  child.stdin.end(prompt);
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    killTree(child);
  }, cfg.timeoutSeconds * 1000);
  const onAbort = () => killTree(child);
  if (signal?.aborted) onAbort();
  signal?.addEventListener('abort', onAbort);
  const code = await new Promise((resolve) => {
    child.on('error', (e) => {
      stderr += `\n${e.message}`;
      resolve(-1);
    });
    child.on('close', resolve);
  });
  clearTimeout(timer);
  signal?.removeEventListener('abort', onAbort);
  const ms = Date.now() - t0;
  const usage = events.find((e) => e.type === 'turn.completed')?.usage;
  try {
    if (signal?.aborted) throw new ModelError('aborted', 'stopped while the model was running', { ms });
    if (timedOut) throw new ModelError('timeout', `the model did not answer within ${cfg.timeoutSeconds}s`, { ms });
    const failures = events.filter((e) => e.type === 'turn.failed' || e.type === 'error').map((e) => e.error?.message || e.message);
    if (code !== 0) {
      const text = [...failures, stderr].join('\n');
      const kind = code === -1 && /ENOENT/.test(stderr) ? 'config' : classify(text);
      throw new ModelError(kind, clip(failures.at(-1) || stderr.trim().split('\n').pop() || `codex exited with code ${code}`, 300), { ms, retryAt: retryAt(text) });
    }
    const out = await fsp.readFile(outFile, 'utf8').catch(() => '');
    const last = out.trim() || events.filter((e) => e.item?.type === 'agent_message').at(-1)?.item?.text || '';
    try {
      return { output: JSON.parse(last), usage, ms };
    } catch {
      throw new ModelError('bad_output', `the model did not return valid JSON (${last.length} chars)`, { ms });
    }
  } finally {
    await fsp.rm(outFile, { force: true }).catch(() => {});
  }
}

/** "try again at 3:45 PM" -> epoch ms, if the message says when. */
function retryAt(text) {
  const m = String(text).match(/try again (?:at|after) ([^.\n]+)/i);
  if (!m) return null;
  const t = Date.parse(m[1]);
  if (!Number.isNaN(t)) return t;
  const hm = m[1].match(/(\d{1,2}):(\d{2})\s*(am|pm)?/i);
  if (!hm) return null;
  const d = new Date();
  let h = Number(hm[1]);
  const ap = hm[3]?.toLowerCase();
  if (ap === 'pm' && h < 12) h += 12;
  if (ap === 'am' && h === 12) h = 0;
  d.setHours(h, Number(hm[2]), 0, 0);
  if (d.getTime() < Date.now()) d.setDate(d.getDate() + 1);
  return d.getTime();
}
