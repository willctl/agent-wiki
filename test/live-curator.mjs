// Opt-in live test of the curator against the real model (gpt-6.1-sol by default),
// through the curator's own Codex home and sign-in, on a throwaway wiki. It uses a
// little of your ChatGPT plan's quota, so it only runs when asked:
//
//   $env:AGENT_WIKI_LIVE = "1"; npm.cmd run test:live        (PowerShell)
//   AGENT_WIKI_LIVE=1 npm run test:live                       (bash)
//
// It reads the curator settings (codexPath, codexHome, model, effort) from
// the installed config.json (src/paths.mjs; %APPDATA%\AgentWiki on Windows), so
// run `npm run install-local` and sign the curator in (tray > Curator > Sign in) first.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { appPaths } from '../src/paths.mjs';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { curatorCmd, srvT } from './impl.mjs';

const live = process.env.AGENT_WIKI_LIVE === '1';
const repo = fileURLToPath(new URL('..', import.meta.url));

test('live: the real model files two notes into pages and the log', { skip: !live && 'set AGENT_WIKI_LIVE=1 to run', timeout: 15 * 60_000 }, async () => {
  const installed = JSON.parse(fs.readFileSync(appPaths({ env: Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^AGENT_WIKI_/.test(k))) }).configFile, 'utf8'));
  assert.ok(installed.curator?.codexHome, 'run npm run install-local first');
  const tmp = await fsp.mkdtemp(path.join(os.tmpdir(), 'agent-wiki-live-'));
  const home = path.join(tmp, 'home');
  const wikiDir = path.join(tmp, 'wiki');
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ writeMode: 'curated', curator: { ...installed.curator } }));
  const env = { ...process.env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: home };
  delete env.AGENT_WIKI_WRITE_MODE;
  const runtime = path.join(repo, 'dist', 'runtime');

  const client = new Client({ name: 'live-test', version: '1' });
  await client.connect(new StdioClientTransport({ ...srvT(runtime), env, stderr: 'pipe' }));
  try {
    for (const args of [
      { app: 'claude-code', title: 'Started Orchard, a recipe-scaling CLI', body: 'Repo C:/Projects/orchard, TypeScript, Node 24. Decided on SQLite (better-sqlite3) for the pantry list because it needs no server.', tags: ['orchard'] },
      { app: 'chatgpt-desktop', title: 'Orchard moved to Postgres', body: 'Orchard now uses Postgres 17 instead of SQLite, because the pantry list is shared with the family web app. Database runs in docker compose (service "db").' },
    ]) {
      const r = await client.callTool({ name: 'wiki_log', arguments: args });
      assert.ok(!r.isError, r.content[0].text);
    }
  } finally {
    await client.close();
  }

  const t0 = Date.now();
  const proc = spawn(...curatorCmd(runtime, '--once'), { env, stdio: ['ignore', 'inherit', 'inherit'] });
  const code = await new Promise((r) => proc.on('exit', r));
  assert.equal(code, 0);
  console.log(`curator finished in ${Math.round((Date.now() - t0) / 1000)} s`);

  assert.deepEqual(fs.readdirSync(path.join(wikiDir, 'inbox')), [], 'both notes filed (if not: see the curator status and the request log)');
  const pages = fs.readdirSync(path.join(wikiDir, 'pages'));
  assert.ok(pages.length >= 1, 'at least one page');
  const text = pages.map((p) => fs.readFileSync(path.join(wikiDir, 'pages', p), 'utf8')).join('\n\n');
  assert.match(text, /Postgres/);
  assert.match(text, /SQLite/, 'the superseded fact is kept');
  const audits = [];
  const walk = (d) => fs.readdirSync(d, { withFileTypes: true }).forEach((e) => (e.isDirectory() ? walk(path.join(d, e.name)) : audits.push(path.join(d, e.name))));
  walk(path.join(wikiDir, '.curator', 'audit'));
  assert.equal(audits.length, 2);
  const audit = JSON.parse(fs.readFileSync(audits[0], 'utf8'));
  assert.equal(audit.model, installed.curator.model);
  console.log(`\n--- pages (${pages.join(', ')}) ---\n${text}\n--- log ---\n${fs.readFileSync(path.join(wikiDir, 'log', String(new Date().getFullYear()), fs.readdirSync(path.join(wikiDir, 'log', String(new Date().getFullYear())))[0]), 'utf8')}`);
  console.log(`(kept for inspection: ${wikiDir})`);
});
