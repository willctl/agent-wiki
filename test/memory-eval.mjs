// The memory eval's deterministic part (scripts/eval.mjs, docs/memory-eval.md): search quality floors on
// the synthetic wiki, and the Ask and curator pipelines end to end with the fake model. The live run
// (`npm run eval -- --live`) gives the quality numbers; these guard against regressions on every OS.

import assert from 'node:assert/strict';
import { describe, test } from 'node:test';
import { loadSet, missingFacts, relOf, runEval, sectionText } from '../scripts/eval.mjs';
import { IMPL } from './impl.mjs';

describe('memory eval', { timeout: 600_000 }, () => {
  test('the synthetic set is consistent: every evidence file exists, every category is covered', async () => {
    const fs = await import('node:fs');
    const path = await import('node:path');
    const set = loadSet('synthetic');
    const cats = new Set(set.questions.map((q) => q.category));
    for (const c of ['extraction', 'multi-session', 'temporal', 'knowledge-update', 'abstention']) assert.ok(cats.has(c), c);
    for (const q of set.questions) {
      if (q.abstain) continue;
      assert.ok(q.facts?.length && q.evidence?.length && q.query, q.id);
      for (const e of q.evidence) assert.ok(fs.existsSync(path.join(set.wikiSrc, e)), `${q.id}: ${e}`);
      // Every expected fact is really in one of its evidence files.
      const text = q.evidence.map((e) => fs.readFileSync(path.join(set.wikiSrc, e), 'utf8')).join('\n');
      assert.deepEqual(missingFacts(text, q.facts.filter((f) => !/^(two|concurrent|both|60 s|every minute)/i.test(f))), [], q.id);
    }
    assert.equal(new Set(set.questions.map((q) => q.id)).size, set.questions.length, 'unique ids');
    assert.equal(new Set(set.streams.map((s) => s.id)).size, set.streams.length, 'unique stream ids');
  });

  test('scoring helpers', () => {
    assert.deepEqual(missingFacts('Repo: C:\\Projects\\harbor on port 8443', ['c:/projects/harbor', '8443|9443', '16']), ['16']);
    assert.equal(relOf('harbor', 'page'), 'pages/harbor.md');
    assert.equal(relOf('2026-09-21', 'log'), 'log/2026/2026-09-21.md');
    assert.equal(relOf('inbox/x.md', 'note'), 'inbox/x.md');
    assert.equal(sectionText('# T\n\n## Open items\n\n- a\n\n## Next\n\n- b', 'open items').trim(), '- a');
  });

  test('search floors, and the Ask and curator pipelines with the fake model', async () => {
    const r = await runEval({ set: 'synthetic', tiers: ['search', 'ask', 'curate'], live: false, concurrency: 4 });
    const s = r.tiers.search.summary;
    // Floors a little under the values with 77 questions. Before M5 (2026-10-03): question 78.8/97/100,
    // keywords 89.4/100/100. With M5's section-level BM25 and dates: question 83.3/98.5/100, keywords
    // 90.9/100/100. The Node runtime keeps the old scoring until it is retired.
    const m5 = IMPL === 'rust';
    assert.ok(s.question['recall@1'] >= (m5 ? 80 : 75), `question recall@1 ${s.question['recall@1']}`);
    assert.ok(s.question['recall@3'] >= (m5 ? 95 : 90), `question recall@3 ${s.question['recall@3']}`);
    assert.ok(s.question['recall@8'] >= 95, `question recall@8 ${s.question['recall@8']}`);
    assert.ok(s.keywords['recall@1'] >= (m5 ? 88 : 85), `keywords recall@1 ${s.keywords['recall@1']}`);
    assert.ok(s.keywords['recall@3'] >= 95, `keywords recall@3 ${s.keywords['recall@3']}`);

    const a = r.tiers.ask.summary;
    assert.equal(a.answered, a.n, `every question answered: ${JSON.stringify(r.tiers.ask.rows.filter((x) => x.status !== 'done').map((x) => [x.id, x.error]))}`);
    // It opens only the top hit of one search: 77.3% with the 77 questions of 2026-10-03.
    assert.ok(a.evidenceRead >= 70, `the naive fake agent reads the evidence (${a.evidenceRead}%)`);

    const c = r.tiers.curate;
    assert.equal(c.rows.length, loadSet('synthetic').streams.length);
    for (const row of c.rows) assert.equal(row.curatorExit, 0, `${row.id}: ${row.curatorTail}`);
    const byId = Object.fromEntries(c.rows.map((x) => [x.id, x]));
    assert.ok(byId.secret.pass, 'the server refuses a note with a secret');
    assert.ok(byId['update-port'].checks[0].pass, 'a filed update reaches the page');
    assert.ok(!byId['update-port'].checks[2].pass, 'the fake model keeps no History section, and the check sees that');
    assert.ok(byId['page-cap'].checks[4].pass, "a stream's own pages are set up before it runs");
    // The fake model files every note as told, so it is expected to fail the poisoning and triage checks:
    // that shows the checks can fail. Only the live model's numbers say anything about the curator.
    assert.ok(!byId['poison-instruction'].pass, 'the checks catch a curator that files injected text');
  });
});
