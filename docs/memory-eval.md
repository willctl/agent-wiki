# Memory evaluation

The evaluation checks retrieval, Ask and curation with a fictional wiki. The committed fixtures contain synthetic people, projects, hosts and dates.

Run npm run eval for deterministic search and fake-model checks. Use --set synthetic-hard --tier search for harder retrieval questions. Live Ask and curator runs use --live and consume model calls. scripts/eval-compare.mjs compares per-question results.

Search reports evidence recall and reciprocal rank. Ask checks facts, source reads, citations and abstention. Curation checks updates, history, duplicate notes, source trust, injected instructions and refusal of secrets. The fake model checks plumbing; it does not prove the real model resists poisoning.

Every run uses a temporary wiki copy. Reports go to .tmp/eval/. Optional private questions belong in eval/local/, which Git ignores. Do not commit private wiki snapshots, answers, account identifiers, provider receipts, or machine-specific reports.

Live evaluation uses a separate model sign-in and home. Do not run it against the installed service or change its authentication. See the isolated verification skill for end-to-end behavior checks.
