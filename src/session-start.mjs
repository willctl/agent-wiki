// SessionStart hook: prints {"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":...}}.
// A broken or unreachable wiki must never block a session, so:
//  - stdin is drained asynchronously and never waited on (hosts may not close it);
//  - the wiki is read by a child process (`--collect`). A read stuck on a hung
//    path blocks a libuv threadpool thread, and process.exit() joins that pool,
//    so a watchdog in the same process can print but can never exit. The
//    supervisor does no filesystem I/O and simply abandons a slow child;
//  - a 5 s watchdog emits a fallback context;
//  - we exit only from the stdout write callback (pipes are async on Windows);
//  - any error emits the fallback and exits 0.

import { spawn } from 'node:child_process';
import fsp from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import * as wiki from './wiki.mjs';

const WATCHDOG_MS = 5000;
const RULES =
  "Agent Wiki is the user's shared long-term memory across Claude, ChatGPT and other AI apps, reached through the " +
  'agent-wiki tools. Call wiki_start once before your first substantive reply and follow the protocol it returns; ' +
  'search the wiki before asking for context the user may have given before, and tell it what happened (decisions, ' +
  'outcomes, preferences, facts, follow-ups) with wiki_log; plain notes are fine, a curator files them into pages ' +
  '(never secrets).';
const FALLBACK =
  "Agent Wiki (the user's shared memory across AI apps) is installed but could not be read just now; " +
  'call wiki_start if the tools are available.';

async function collect() {
  const { wikiDir } = await wiki.resolveWikiDir();
  const st = await fsp.stat(wikiDir);
  if (!st.isDirectory()) throw new Error('wiki path is not a directory');
  const [pages, headlines] = await Promise.all([wiki.listPages(wikiDir), wiki.recentHeadlines(wikiDir, 3, 15)]);
  const slugs = pages.map((p) => p.slug);
  const shown = slugs.slice(0, 80);
  const more = slugs.length > shown.length ? `, ...and ${slugs.length - shown.length} more` : '';
  const lines = [
    `${RULES} Wiki folder: ${wiki.displayPath(wikiDir)}.`,
    '',
    `Pages (${slugs.length}): ${shown.length ? shown.join(', ') : '(none yet)'}${more}`,
    '',
    headlines.length ? 'Recent activity (last 3 days, newest first):' : 'Recent activity (last 3 days): none.',
    ...headlines.map((h) => `- ${h.date} ${h.time} · ${h.text}`),
  ];
  return lines.join('\n');
}

function runCollector() {
  collect().then(
    (text) => process.stdout.write(text, () => process.exit(0)),
    () => process.exit(1),
  );
}

function runSupervisor() {
  let done = false;
  let child = null;
  const emit = (additionalContext) => {
    if (done) return;
    done = true;
    clearTimeout(watchdog);
    const out = JSON.stringify({ hookSpecificOutput: { hookEventName: 'SessionStart', additionalContext } });
    process.stdout.write(`${out}\n`, () => process.exit(0));
  };
  const watchdog = setTimeout(() => {
    try {
      child?.kill();
    } catch {
      // abandoned either way
    }
    emit(FALLBACK);
  }, WATCHDOG_MS);

  process.stdin.on('error', () => {});
  process.stdin.on('data', () => {});
  process.stdin.resume();
  process.stdout.on('error', () => process.exit(0));

  try {
    let out = '';
    child = spawn(process.execPath, [fileURLToPath(import.meta.url), '--collect'], {
      stdio: ['ignore', 'pipe', 'ignore'],
      windowsHide: true,
    });
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (d) => (out += d));
    child.on('error', () => emit(FALLBACK));
    child.on('close', (code) => emit(code === 0 && out.trim() ? out.trim() : FALLBACK));
  } catch {
    emit(FALLBACK);
  }
}

// 'stop': the Stop hook, which only the Rust program implements (an opt-in nudge); say nothing.
if (process.argv.includes('stop')) process.exit(0);
else if (process.argv.includes('--collect')) runCollector();
else runSupervisor();
