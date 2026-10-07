// A stand-in for the Codex CLI, used by the curator tests. It accepts the same
// arguments the curator passes to `codex exec` / `codex login status`, reads the
// prompt from stdin, and answers with a deterministic edit plan written to the
// -o file, plus JSONL events on stdout, like the real CLI.
//
// FAKE_CODEX_MODE (comma-separated):
//   normal (default)   file each note: patch/create the page it names, one log entry per note;
//                      a note titled "FORGET ..." asks to forget its body (plan.forget);
//                      a page review (lint prompt) replaces "LINTME" with "tidied", else changes nothing
//   leak-secret        put an AWS-key-looking string into every page change
//   bad-json           write something that is not JSON
//   rate-limit         fail like a ChatGPT usage limit
//   signed-out         exec fails with 401; `login status` says not logged in
//   wrong-hash-once    the first plan uses a wrong base hash (exercises the repair round)
//   touch-page-once    the first call edits the target page on disk before answering (a human edit mid-plan)
//   ask-write          (Ask) also try wiki_log, which the read-only server must not offer
//   refuse-model=<m>   exec with -m <m> fails the way ChatGPT refuses a model the account does not offer
// FAKE_CODEX_DELAY_MS  sleep before answering (Ask: before each step)
// FAKE_CODEX_LOG       append {args, codexHome, apiKey} per call (JSON lines)
// FAKE_CODEX_STATE     a file used to count calls (for the "once" modes)
// FAKE_CODEX_PROMPTS   a folder: each Ask prompt is saved there as <n>.txt
//
// A prompt with a <question> block is an Ask: like the real CLI, the fake starts the MCP server
// given by `-c mcp_servers.wiki.*` (with only that env table on top of a minimal environment),
// searches the wiki for the question's words, reads the best hit and answers from it, streaming
// mcp_tool_call events as it goes.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { getDefaultEnvironment, StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import fs from 'node:fs';
import path from 'node:path';

const args = process.argv.slice(2);
// FAKE_CODEX_MODE_FILE, when present, overrides FAKE_CODEX_MODE (and may say delay=<ms>): a test can switch modes between the calls of one long-running curator.
const modeText = (() => {
  try {
    return fs.readFileSync(process.env.FAKE_CODEX_MODE_FILE, 'utf8');
  } catch {
    return process.env.FAKE_CODEX_MODE || 'normal';
  }
})();
const modes = new Set(modeText.split(',').map((s) => s.trim()));
const delayMs = Number([...modes].find((m) => m.startsWith('delay='))?.slice(6) ?? process.env.FAKE_CODEX_DELAY_MS ?? 0);
const opt = (n) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : undefined;
};
if (process.env.FAKE_CODEX_LOG) {
  fs.appendFileSync(
    process.env.FAKE_CODEX_LOG,
    `${JSON.stringify({ args, codexHome: process.env.CODEX_HOME, apiKey: Boolean(process.env.CODEX_API_KEY || process.env.OPENAI_API_KEY) })}\n`,
  );
}

if (args[0] === 'login' && args[1] === 'status') {
  if (modes.has('signed-out')) {
    process.stderr.write('Not logged in\n');
    process.exit(1);
  }
  process.stderr.write('Logged in using ChatGPT\n');
  process.exit(0);
}
if (args[0] !== 'exec') {
  process.stderr.write(`fake codex: unsupported ${args.join(' ')}\n`);
  process.exit(2);
}

const event = (e) => process.stdout.write(`${JSON.stringify(e)}\n`);
let calls = 0;
if (process.env.FAKE_CODEX_STATE) {
  try {
    calls = Number(fs.readFileSync(process.env.FAKE_CODEX_STATE, 'utf8')) || 0;
  } catch {
    calls = 0;
  }
  fs.writeFileSync(process.env.FAKE_CODEX_STATE, String(calls + 1));
}
const firstCall = calls === 0;

let prompt = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (d) => (prompt += d));
process.stdin.on('end', async () => {
  if (delayMs) await new Promise((r) => setTimeout(r, delayMs));
  event({ type: 'thread.started', thread_id: 'fake' });
  event({ type: 'turn.started' });
  if (modes.has('signed-out')) {
    event({ type: 'error', message: 'unexpected status 401 Unauthorized: Your access token could not be refreshed. Please log out and sign in again.' });
    event({ type: 'turn.failed', error: { message: 'unexpected status 401 Unauthorized' } });
    process.exit(1);
  }
  const refused = [...modes].find((m) => m.startsWith('refuse-model='))?.slice(13);
  if (refused && args[args.indexOf('-m') + 1] === refused) {
    event({ type: 'error', message: `{"detail":"The '${refused}' model is not supported when using Codex with a ChatGPT account."}` });
    event({ type: 'turn.failed', error: { message: 'model not supported' } });
    process.exit(1);
  }
  if (modes.has('rate-limit')) {
    event({ type: 'error', message: 'You’ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 11:59 PM.' });
    event({ type: 'turn.failed', error: { message: 'usage limit' } });
    process.exit(1);
  }
  const question = prompt.match(/<question>\n([\s\S]*?)\n<\/question>/);
  if (question) {
    await ask(question[1]);
    return;
  }
  const m = prompt.match(/<input>\n([\s\S]*?)\n<\/input>/);
  if (!m) {
    event({ type: 'turn.failed', error: { message: 'fake codex: no <input> block in the prompt' } });
    process.exit(1);
  }
  const input = JSON.parse(m[1]);
  const out = opt('-o');
  if (modes.has('bad-json')) {
    fs.writeFileSync(out, 'I could not decide. {not json');
    event({ type: 'turn.completed', usage: { input_tokens: 10, output_tokens: 5 } });
    process.exit(0);
  }

  if (input.page && !input.notes) {
    // A page review (the lint): tidy a "LINTME" marker if the page has one, else change nothing.
    const p = input.page;
    const pages = p.body.includes('LINTME')
      ? [{ slug: p.slug, action: 'patch', base_hash: p.hash, title: null, type: null, summary: null, tags: null, content: null, edits: [{ find: 'LINTME', replace: 'tidied' }], note_ids: [], reason: 'a stale marker' }]
      : [];
    const plan = { notes: [], pages, log: [], forget: [], summary: pages.length ? 'Tidied a marker.' : 'No changes needed.' };
    fs.writeFileSync(out, JSON.stringify(plan));
    event({ type: 'turn.completed', usage: { input_tokens: prompt.length, output_tokens: 20 } });
    process.exit(0);
  }
  const shown = new Map(input.pages.map((p) => [p.slug, p]));
  const indexed = new Set(input.index.map((p) => p.slug));
  const groups = new Map();
  const notes = [];
  const forget = [];
  for (const n of input.notes) {
    if (/IGNORE/.test(n.title)) {
      notes.push({ id: n.id, disposition: 'ignored', reason: 'chatter' });
      continue;
    }
    if (/^FORGET\b/.test(n.title)) {
      // "FORGET ..." notes ask to forget their body text (the curator's forget list).
      notes.push({ id: n.id, disposition: 'log_only', reason: 'a request to forget' });
      forget.push({ text: n.body.trim(), note_ids: [n.id] });
      continue;
    }
    const slug = n.page?.slug || n.pages[0] || 'misc-notes';
    if (indexed.has(slug) && !shown.has(slug)) {
      notes.push({ id: n.id, disposition: 'log_only', reason: 'page not shown' });
      continue;
    }
    notes.push({ id: n.id, disposition: 'integrated', reason: `filed into ${slug}` });
    if (!groups.has(slug)) groups.set(slug, []);
    groups.get(slug).push(n);
  }
  const leak = modes.has('leak-secret') ? `\nkey ${'AKIA'}${'IOSFODNN7EXAMPLE'}` : '';
  const pages = [];
  for (const [slug, ns] of groups) {
    const bullets = ns.map((n) => `- ${n.title}: ${n.body.split('\n')[0]}`).join('\n');
    const cur = shown.get(slug);
    const base = { slug, title: null, type: null, summary: null, tags: null, content: null, edits: null, base_hash: null, note_ids: ns.map((n) => n.id), reason: 'notes about this subject' };
    if (cur) {
      if (modes.has('touch-page-once') && firstCall) {
        // Someone edits the page while the model is thinking.
        const file = path.join(process.env.AGENT_WIKI_DIR, 'pages', `${slug}.md`);
        fs.writeFileSync(file, `${fs.readFileSync(file, 'utf8').trimEnd()}\n\nHuman edit while planning.\n`);
      }
      const heading = cur.body.split('\n')[0];
      pages.push({
        ...base,
        action: 'patch',
        base_hash: modes.has('wrong-hash-once') && firstCall ? '0000000000000000' : cur.hash,
        edits: [{ find: heading, replace: `${heading}\n\n${bullets}${leak}` }],
      });
    } else {
      const title = ns[0].page?.title || slug.replace(/-/g, ' ').replace(/^./, (c) => c.toUpperCase());
      pages.push({
        ...base,
        action: 'create',
        title,
        type: ns[0].page?.type || 'topic',
        summary: `Notes about ${title}`,
        tags: [...new Set(ns.flatMap((n) => n.tags))],
        content: `# ${title}\n\n${bullets}${leak}`,
      });
    }
  }
  const log = input.notes
    .filter((n) => !/IGNORE/.test(n.title))
    .map((n) => (/^FORGET\b/.test(n.title) ? { note_ids: [n.id], title: 'Asked to forget something', body: '', tags: [], pages: [] } : { note_ids: [n.id], title: n.title, body: n.body, tags: n.tags, pages: [n.page?.slug || n.pages[0] || 'misc-notes'] }));
  const plan = { notes, pages, log, forget, summary: `Filed ${input.notes.length} note(s).` };
  fs.writeFileSync(out, JSON.stringify(plan));
  event({ type: 'item.completed', item: { id: 'item_0', type: 'agent_message', text: JSON.stringify(plan) } });
  event({ type: 'turn.completed', usage: { input_tokens: prompt.length, cached_input_tokens: 0, output_tokens: 100, reasoning_output_tokens: 0 } });
});

// ---------------------------------------------------------------- Ask

/** `-c key=value` pairs, values parsed the way our TOML is written (JSON strings, arrays, inline tables). */
function configValue(key) {
  for (let i = 0; i < args.length - 1; i++) {
    if (args[i] !== '-c' || !args[i + 1].startsWith(`${key}=`)) continue;
    const v = args[i + 1].slice(key.length + 1);
    return JSON.parse(v.trim().startsWith('{') ? v.replace(/([{,]\s*)([A-Za-z_][A-Za-z0-9_]*)=/g, '$1"$2":') : v);
  }
  return undefined;
}

async function ask(question) {
  if (process.env.FAKE_CODEX_PROMPTS) {
    fs.mkdirSync(process.env.FAKE_CODEX_PROMPTS, { recursive: true });
    fs.writeFileSync(path.join(process.env.FAKE_CODEX_PROMPTS, `${fs.readdirSync(process.env.FAKE_CODEX_PROMPTS).length + 1}.txt`), prompt);
  }
  const delay = () => new Promise((r) => setTimeout(r, delayMs));
  const client = new Client({ name: 'codex-mcp-client', version: 'fake' });
  await client.connect(
    new StdioClientTransport({ command: configValue('mcp_servers.wiki.command'), args: configValue('mcp_servers.wiki.args'), env: { ...getDefaultEnvironment(), ...configValue('mcp_servers.wiki.env') }, stderr: 'ignore' }),
  );
  const offered = (await client.listTools()).tools.map((t) => t.name).sort();
  let n = 0;
  const call = async (tool, a) => {
    const id = `item_${++n}`;
    event({ type: 'item.started', item: { id, type: 'mcp_tool_call', server: 'wiki', tool, arguments: a, result: null, error: null, status: 'in_progress' } });
    await delay();
    let result;
    let error = null;
    try {
      result = await client.callTool({ name: tool, arguments: a });
    } catch (e) {
      error = { message: e.message };
    }
    event({ type: 'item.completed', item: { id, type: 'mcp_tool_call', server: 'wiki', tool, arguments: a, result: result ?? null, error, status: error ? 'failed' : 'completed' } });
    return result?.content?.map((c) => c.text).join('\n') || '';
  };
  event({ type: 'item.completed', item: { id: 'item_0', type: 'agent_message', text: `Tools: ${offered.join(', ')}. Looking in the wiki.` } });
  if (modes.has('ask-write')) await call('wiki_log', { app: 'fake', title: 'should not be possible' });
  const words = question.toLowerCase().match(/[a-z0-9-]{4,}/g) || [];
  const found = await call('wiki_search', { query: words.join(' ') });
  const target = found.match(/\(read: "([^"]+)"/)?.[1];
  let line = '';
  if (target) {
    const text = await call('wiki_read', { target });
    line = text.split('\n').find((l) => !l.startsWith('File:') && words.some((w) => l.toLowerCase().includes(w)) && !/^(title|summary|tags):/.test(l)) || '';
  }
  await client.close();
  const output = target
    ? { answer: `${line.trim() || 'It is in the wiki.'}\n\nSee [[${target}]].`, found: true, sources: [{ target, quote: line.trim().slice(0, 150) }] }
    : { answer: 'The wiki does not say.', found: false, sources: [] };
  if (modes.has('bad-json')) fs.writeFileSync(opt('-o'), '{not json');
  else fs.writeFileSync(opt('-o'), JSON.stringify(output));
  event({ type: 'item.completed', item: { id: `item_${++n}`, type: 'agent_message', text: JSON.stringify(output) } });
  event({ type: 'turn.completed', usage: { input_tokens: prompt.length, cached_input_tokens: 0, output_tokens: 50, reasoning_output_tokens: 0 } });
}