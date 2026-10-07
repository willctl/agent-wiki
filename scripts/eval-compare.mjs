// Compares two eval reports question by question (the same set, two variants: BM25 against hybrid,
// one prompt against another) and says whether the difference is more than chance.
//
//   node scripts/eval-compare.mjs <before.json> <after.json>
//
// For each measure it counts the questions only one side got right and runs an exact McNemar test
// (a two-sided binomial test on those discordant questions). Questions both sides got right or wrong
// say nothing about the difference, so a gain needs enough discordant questions in one direction:
// with 6 discordant questions all in one direction the p-value is 0.031; with 5 it is 0.063.

import fs from 'node:fs';

const [a, b] = process.argv.slice(2).map((f) => JSON.parse(fs.readFileSync(f, 'utf8')));
if (!a || !b) {
  console.error('usage: node scripts/eval-compare.mjs <before.json> <after.json>');
  process.exit(2);
}
if (a.set !== b.set) console.error(`warning: different sets (${a.set}, ${b.set}): only questions in both are compared`);

/** Two-sided exact binomial p-value for k of n under p = 0.5. */
function mcnemar(n, k) {
  if (n === 0) return 1;
  const lo = Math.min(k, n - k);
  let tail = 0;
  let c = 1; // C(n, 0)
  for (let i = 0; i <= lo; i++) {
    tail += c;
    c = (c * (n - i)) / (i + 1);
  }
  return Math.min(1, (2 * tail) / 2 ** n);
}

function compare(name, before, after) {
  const ids = Object.keys(before).filter((id) => id in after);
  if (!ids.length) return;
  const gained = ids.filter((id) => !before[id] && after[id]);
  const lost = ids.filter((id) => before[id] && !after[id]);
  const count = (m) => ids.filter((id) => m[id]).length;
  const p = mcnemar(gained.length + lost.length, gained.length);
  const verdict = p < 0.05 ? (gained.length > lost.length ? 'better' : 'worse') : 'no clear difference';
  console.log(`${name.padEnd(34)} ${String(count(before)).padStart(3)} -> ${String(count(after)).padStart(3)} of ${ids.length}   +${gained.length} -${lost.length}   p=${p.toFixed(3)}   ${verdict}`);
  if (gained.length) console.log(`  gained: ${gained.join(', ')}`);
  if (lost.length) console.log(`  lost:   ${lost.join(', ')}`);
}

const hits = (r, mode, k) => Object.fromEntries((r.tiers.search?.rows || []).map((x) => [x.id, !!x[mode] && x[mode] <= k]));
for (const mode of ['question', 'keywords']) {
  for (const k of [1, 3, 8]) compare(`search, ${mode === 'question' ? 'as asked' : 'good words'}, top ${k}`, hits(a, mode, k), hits(b, mode, k));
}
const route = (r) => Object.fromEntries((r.tiers.search?.routing || []).map((x) => [x.id, !!x.rank]));
compare('routing, page in the top 4', route(a), route(b));
const ask = (r, key) => Object.fromEntries((r.tiers.ask?.rows || []).map((x) => [x.id, !!x[key]]));
compare('ask, facts right', ask(a, 'factsOk'), ask(b, 'factsOk'));
compare('ask, judged right', ask(a, 'judged'), ask(b, 'judged'));
