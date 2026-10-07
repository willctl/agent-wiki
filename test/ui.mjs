// The tray window: the activity-log parser, the safe Markdown renderer, and the /ui + /api
// endpoints of the BUNDLED server (static files with CSP, JSON API, the pause guard, and the same
// Host/Origin refusals as /mcp), against a temp wiki.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { after, before, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { parseLogDay } from '../src/activity.mjs';
import { createRenderer, escapeHtml, highlight, plainText, snippetAround } from '../ui/src/markdown.js';
import { IMPL, srv } from './impl.mjs';

const repo = fileURLToPath(new URL('..', import.meta.url));
const dist = path.join(repo, 'dist', 'runtime');
if (!fs.existsSync(path.join(dist, 'ui', 'app.js'))) throw new Error('Run `npm run build` first.');

describe('activity log parser', () => {
  const DAY = [
    '# 2026-10-02',
    '',
    '## 13:27 · codex · Atlas recovered after reboot',
    '',
    'tags: atlas, windows  ',
    'pages: [[atlas]], [[example-workstation]]',
    '',
    'Started Docker. See [[docker-notes]] too.',
    '',
    'Second paragraph.',
    '',
    '- 13:53 · curator · Page updated: Agent Wiki [[agent-wiki]]',
    '- 13:53 · curator · Page created: Orchard [[orchard]]',
    '',
    '<!-- curator batch 2026-10-02_13-50-36-712-9195bd -->',
    '',
    '## 14:30 · claude-code · Checked the tray',
    '',
    'pages: [[agent-wiki]]',
    '',
    'Checked it.',
    'tags: not-a-tag-line-because-body-started',
  ].join('\n');

  test('full and compact entries, newest first, with tags, pages and body', () => {
    const e = parseLogDay('2026-10-02', DAY);
    assert.deepEqual(
      e.map((x) => [x.time, x.app, x.compact]),
      [['14:30', 'claude-code', false], ['13:53', 'curator', true], ['13:53', 'curator', true], ['13:27', 'codex', false]],
    );
    const codex = e[3];
    assert.equal(codex.title, 'Atlas recovered after reboot');
    assert.deepEqual(codex.tags, ['atlas', 'windows']);
    assert.deepEqual(codex.pages, ['atlas', 'example-workstation', 'docker-notes'], 'pages line first, then links in the body');
    assert.equal(codex.body, 'Started Docker. See [[docker-notes]] too.\n\nSecond paragraph.');
    assert.deepEqual(e[1].pages, ['orchard']);
    assert.equal(e[1].title, 'Page created: Orchard [[orchard]]');
    assert.deepEqual(e[0].tags, [], 'a tags: line after the body started is body text');
    assert.equal(e[0].body, 'Checked it.\ntags: not-a-tag-line-because-body-started');
    assert.deepEqual(parseLogDay('2026-10-02', ''), []);
  });
});

describe('markdown rendering', () => {
  const render = createRenderer({ known: (s) => s === 'atlas' });
  test('[[wikilinks]] link inside the window; unknown pages are marked', () => {
    const html = render('See [[atlas]] and [[new-page|the new page]].');
    assert.match(html, /<a class="wikilink" href="#\/page\/atlas" data-slug="atlas">atlas<\/a>/);
    assert.match(html, /<a class="wikilink missing" href="#\/page\/new-page" data-slug="new-page">the new page<\/a>/);
    const titled = createRenderer({ known: () => true, title: (s) => (s === 'atlas' ? 'Atlas <dev>' : '') });
    assert.match(titled('[[atlas]], [[atlas|it]] and [[other]]'), /data-slug="atlas">Atlas &lt;dev&gt;<\/a>, .*data-slug="atlas">it<\/a> and .*data-slug="other">other<\/a>/, 'answers show titles, escaped');
  });
  test('nothing in a page runs: raw HTML is text, unsafe links are dropped, remote images become links', () => {
    const html = render('<script>alert(1)</script>\n\nHi <img src=x onerror=alert(1)> there\n\n[a](javascript:alert(1)) [b](https://example.com "t") ![pic](https://example.com/x.png) [c](#/status)');
    assert.ok(!/<script|<img/i.test(html), html);
    assert.match(html, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
    assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
    assert.ok(!/javascript:/i.test(html.replace(/&lt;[^&]*&gt;/g, '')), 'no javascript: href');
    assert.match(html, /<a href="https:\/\/example\.com" title="t" target="_blank" rel="noopener noreferrer">b<\/a>/);
    assert.match(html, /<a href="https:\/\/example\.com\/x\.png" target="_blank" rel="noopener noreferrer">pic<\/a>/);
    assert.match(html, /<a href="#\/status">c<\/a>/);
    const hrefs = [...render('[ok](https://e.example) [a](JAVASCRIPT:x) [b]( javascript:x) [c](data:text/html,x) [d](vbscript:x) [e](file:///etc/passwd)').matchAll(/href="([^"]*)"/g)].map((m) => m[1]);
    assert.deepEqual(hrefs, ['https://e.example'], 'only safe link schemes survive');
  });
  test('tables, code and lists render (GFM)', () => {
    const html = render('| a | b |\n| - | - |\n| 1 | 2 |\n\n```\n<x>\n```\n\n- one\n- two');
    assert.match(html, /<table>/);
    assert.match(html, /<code>&lt;x&gt;\n<\/code>/);
    assert.match(html, /<li>one<\/li>/);
  });
  test('search helpers: highlight escapes then marks; snippets centre on the first match', () => {
    assert.equal(highlight('a <b> Tray tray', ['tray']), 'a &lt;b&gt; <mark>Tray</mark> <mark>tray</mark>');
    assert.equal(highlight('x', []), 'x');
    assert.equal(escapeHtml(`"'&`), '&quot;&#39;&amp;');
    const long = `${'word '.repeat(60)}the logon task is here ${'tail '.repeat(40)}`;
    const s = snippetAround(long, ['logon'], 80);
    assert.ok(s.startsWith('…') && s.endsWith('…'), s);
    assert.ok(s.includes('logon task'), s);
    assert.ok(s.length <= 82, `${s.length}`);
    assert.equal(snippetAround('short [[slug|text]] **bold**', ['x']), 'short text bold');
    assert.equal(plainText('# T\n\nSee [[a-b|A B]] and [x](http://y).', 100), 'T See A B and x.');
    assert.equal(plainText('> call `wiki_start` and _then_ wiki_log', 100), 'call wiki_start and then wiki_log', 'snake_case names keep their underscores');
  });
});

describe('the /ui and /api endpoints', { timeout: 60_000 }, () => {
  let tmp;
  let wikiDir;
  let server;
  let port;
  before(async () => {
    tmp = fs.realpathSync.native(await fsp.mkdtemp(path.join(os.tmpdir(), 'agent-wiki-ui-')));
    wikiDir = path.join(tmp, 'wiki');
    const home = path.join(tmp, 'home');
    fs.mkdirSync(path.join(wikiDir, 'pages'), { recursive: true });
    fs.mkdirSync(home, { recursive: true });
    fs.writeFileSync(path.join(home, 'config.json'), JSON.stringify({ writeMode: 'curated' }));
    const page = (slug, title, type, body) =>
      fs.writeFileSync(path.join(wikiDir, 'pages', `${slug}.md`), `---\ntitle: ${title}\ntype: ${type}\nsummary: About ${title}\ntags: ["t1"]\nupdated: 2026-10-02T10:00:00-05:00\nupdated_by: curator\n---\n\n# ${title}\n\n${body}\n`);
    page('alpha', 'Alpha', 'project', 'Links to [[beta]] and [[ghost]]. The tray logon task lives here.');
    page('beta', 'Beta', 'reference', 'Plain page.\n\n<!-- curator batch x -->');
    fs.mkdirSync(path.join(wikiDir, '.history', 'pages', 'alpha'), { recursive: true });
    fs.writeFileSync(path.join(wikiDir, '.history', 'pages', 'alpha', '2026-10-01_14-53-26-523.md'), 'old');
    const d = new Date();
    const date = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
    fs.mkdirSync(path.join(wikiDir, 'log', date.slice(0, 4)), { recursive: true });
    fs.writeFileSync(path.join(wikiDir, 'log', date.slice(0, 4), `${date}.md`), `# ${date}\n\n## 09:00 · codex · Did a thing\n\ntags: x\n\nBody about [[alpha]].\n`);
    const runtime = path.join(tmp, 'runtime');
    await fsp.cp(dist, runtime, { recursive: true });
    const env = { ...process.env, AGENT_WIKI_DIR: wikiDir, AGENT_WIKI_HOME: home };
    delete env.AGENT_WIKI_WRITE_MODE;
    server = spawn(...srv(runtime, '--http', '--port', '0', '--parent-stdin'), { env, stdio: ['pipe', 'pipe', 'pipe'] });
    port = await new Promise((resolve, reject) => {
      let err = '';
      server.stderr.on('data', (c) => {
        err += c;
        const m = err.match(/127\.0\.0\.1:(\d+)\/mcp/);
        if (m) resolve(Number(m[1]));
      });
      server.on('exit', () => reject(new Error(err)));
    });
  });
  after(async () => {
    server?.kill();
    await fsp.rm(tmp, { recursive: true, force: true, maxRetries: 5 }).catch(() => {});
  });

  const req = (method, p, { headers = {}, body } = {}) =>
    new Promise((resolve, reject) => {
      const r = http.request({ host: '127.0.0.1', port, path: p, method, headers: { Host: `127.0.0.1:${port}`, ...headers } }, (res) => {
        let text = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (text += c));
        res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, text, json: /json/.test(res.headers['content-type'] || '') ? JSON.parse(text) : null }));
      });
      r.on('error', reject);
      if (body !== undefined) r.write(typeof body === 'string' ? body : JSON.stringify(body));
      r.end();
    });

  test('static app: index, bundle and icon, with CSP; nothing outside ui/', async () => {
    assert.equal((await req('GET', '/ui')).status, 308);
    const index = await req('GET', '/ui/');
    assert.equal(index.status, 200);
    assert.match(index.headers['content-type'], /^text\/html/);
    assert.match(index.headers['content-security-policy'], /script-src 'self'/);
    assert.match(index.headers['content-security-policy'], /frame-ancestors 'none'/);
    assert.equal(index.headers['x-content-type-options'], 'nosniff');
    assert.match(index.text, /<title>Agent Wiki<\/title>/);
    const js = await req('GET', '/ui/app.js');
    assert.equal(js.status, 200);
    assert.match(js.headers['content-type'], /^text\/javascript/);
    assert.ok(js.text.length > 10_000);
    assert.equal((await req('GET', '/ui/app.css')).status, 200);
    assert.match((await req('GET', '/ui/icon.svg')).headers['content-type'], /image\/svg\+xml/);
    for (const p of ['/ui/..%2fserver.mjs', '/ui/..%5cserver.mjs', '/ui/%2e%2e/server.mjs', '/ui/.hidden', '/ui/nope.js']) {
      assert.equal((await req('GET', p)).status, 404, p);
    }
    assert.equal((await req('POST', '/ui/')).status, 405);
  });

  test('the same Host and Origin refusals as /mcp', async () => {
    assert.equal((await req('GET', '/ui/', { headers: { Host: 'evil.example' } })).status, 403);
    assert.equal((await req('GET', '/api/pages', { headers: { Host: `attacker.test:${port}` } })).status, 403);
    assert.equal((await req('GET', '/api/pages', { headers: { Origin: 'https://evil.example' } })).status, 403);
    assert.equal((await req('GET', '/api/pages', { headers: { Origin: `http://127.0.0.1:${port}` } })).status, 200);
    // Lookalikes are not this machine: matching must be exact, never a prefix.
    for (const Host of [`localhost.evil.example:${port}`, `127.0.0.1.nip.io:${port}`, `127.0.0.1:${port + 1}`, '127.0.0.1']) {
      assert.equal((await req('GET', '/api/pages', { headers: { Host } })).status, 403, Host);
    }
    for (const Origin of ['null', 'http://127.0.0.1.evil.example', `https://localhost.evil.example:${port}`]) {
      assert.equal((await req('GET', '/api/pages', { headers: { Origin } })).status, 403, Origin);
    }
    if (IMPL === 'rust') {
      for (const Origin of [`http://127.0.0.1:${port + 1}`, `https://127.0.0.1:${port}`, `http://localhost:${port}`, 'http://127.0.0.1']) {
        assert.equal((await req('GET', '/api/pages', { headers: { Origin } })).status, 403, `another local origin: ${Origin}`);
        assert.equal((await req('POST', '/api/curator', { headers: { Origin, 'X-Agent-Wiki': 'ui' }, body: { paused: true } })).status, 403, `mutation from ${Origin}`);
      }
      assert.equal((await req('GET', '/api/pages', { headers: { Host: `localhost:${port}`, Origin: `http://localhost:${port}` } })).status, 200);
      // What a browser says another site's page sent (an <img> needs no CORS approval) is refused.
      for (const site of ['cross-site', 'same-site']) assert.equal((await req('GET', '/api/held', { headers: { 'Sec-Fetch-Site': site } })).status, 403, site);
      for (const site of ['same-origin', 'none']) assert.equal((await req('GET', '/api/pages', { headers: { 'Sec-Fetch-Site': site } })).status, 200, site);
    }
  });

  test('pages, a page with links, backlinks and history, search, activity, a log day', async () => {
    const pages = (await req('GET', '/api/pages')).json.pages;
    assert.deepEqual(pages.map((p) => p.slug).sort(), ['alpha', 'beta']);
    assert.deepEqual(Object.keys(pages[0]).sort(), ['created', 'slug', 'summary', 'tags', 'time', 'title', 'type', 'updated', 'updatedBy', 'words'].sort());
    const alpha = (await req('GET', '/api/page?slug=alpha')).json;
    assert.equal(alpha.title, 'Alpha');
    assert.equal(alpha.updatedBy, 'curator');
    assert.deepEqual(alpha.links, [{ slug: 'beta', exists: true, title: 'Beta' }, { slug: 'ghost', exists: false, title: 'ghost' }]);
    assert.equal(alpha.history.length, 1);
    assert.match(alpha.path, /\/wiki\/pages\/alpha\.md$/);
    const beta = (await req('GET', '/api/page?slug=beta')).json;
    assert.deepEqual(beta.backlinks, [{ slug: 'alpha', title: 'Alpha', type: 'project' }]);
    assert.ok(!beta.body.includes('<!--'), 'curator markers are stripped');
    assert.equal((await req('GET', '/api/page?slug=ghost')).status, 404);
    assert.equal((await req('GET', '/api/page?slug=..%2Fx')).status, 400);
    const s = (await req('GET', '/api/search?q=tray%20logon')).json;
    assert.deepEqual(s.terms, ['tray', 'logon']);
    assert.equal(s.results[0].target, 'alpha');
    assert.deepEqual((await req('GET', '/api/search?q=')).json.results, []);
    const act = (await req('GET', '/api/activity?days=3')).json.days;
    assert.equal(act[0].entries[0].title, 'Did a thing');
    assert.deepEqual(act[0].entries[0].pages, ['alpha']);
    assert.equal((await req('GET', `/api/log?date=${act[0].date}`)).json.entries.length, 1);
    assert.equal((await req('GET', '/api/log?date=1999-01-01')).status, 404);
    assert.equal((await req('GET', '/api/log?date=nope')).status, 400);
    // Digits from another script are not a date (Unicode \d once let them through to a byte slice: 500).
    assert.equal((await req('GET', `/api/log?date=${encodeURIComponent('१२३४-०१-०१')}`)).status, 400);
    assert.equal((await req('GET', '/api/nope')).status, 404);
  });

  test('inbox shows a note queued over MCP; status; pausing needs POST, JSON and the X-Agent-Wiki header', async () => {
    const client = new Client({ name: 'ui-test', version: '1' });
    await client.connect(new StreamableHTTPClientTransport(new URL(`http://127.0.0.1:${port}/mcp`)));
    await client.callTool({ name: 'wiki_log', arguments: { app: 'ui-test', title: 'Queued for the window', body: 'body', pages: ['alpha'] } });
    await client.close();
    const box = (await req('GET', '/api/inbox')).json;
    assert.equal(box.notes.length, 1);
    assert.equal(box.notes[0].title, 'Queued for the window');
    assert.equal(box.notes[0].status, 'pending');
    assert.equal(box.paused, false);
    assert.equal((await req('GET', '/api/status')).json.queue.pending, 1);

    assert.equal((await req('GET', '/api/curator')).status, 405);
    assert.equal((await req('POST', '/api/curator', { body: { paused: true } })).status, 403, 'no X-Agent-Wiki header');
    assert.equal((await req('POST', '/api/curator', { headers: { 'X-Agent-Wiki': '1', 'Content-Type': 'application/json' }, body: { paused: true } })).status, 403, 'only the exact value');
    assert.ok(!fs.existsSync(path.join(wikiDir, '.curator', 'paused')), 'a refused request changes nothing');
    assert.equal((await req('POST', '/api/curator', { headers: { 'X-Agent-Wiki': 'ui', Origin: 'https://evil.example' }, body: { paused: true } })).status, 403);
    assert.equal((await req('POST', '/api/curator', { headers: { 'X-Agent-Wiki': 'ui' }, body: 'not json' })).status, 400);
    const r = await req('POST', '/api/curator', { headers: { 'X-Agent-Wiki': 'ui', 'Content-Type': 'application/json' }, body: { paused: true } });
    assert.deepEqual(r.json, { paused: true });
    assert.ok(fs.existsSync(path.join(wikiDir, '.curator', 'paused')));
    assert.equal((await req('GET', '/api/status')).json.curator.paused, true);
    assert.deepEqual((await req('POST', '/api/curator', { headers: { 'X-Agent-Wiki': 'ui' }, body: { paused: false } })).json, { paused: false });
    assert.ok(!fs.existsSync(path.join(wikiDir, '.curator', 'paused')));
  });
});
