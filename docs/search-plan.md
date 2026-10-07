# Search

Lexical search ranks sections of pages, log entries and notes with BM25-style scoring. Titles, tags, summaries and aliases help rank results. Filters select scope, dates and the originating app. Up to five alternate queries are fused with the primary query.

## Optional embeddings

Hybrid search is disabled unless search.embeddings.enabled is true in the application configuration. It embeds changed sections and search queries through OpenRouter, then combines cosine similarity with lexical scores. A failed or slow embedding request falls back to lexical search.

The default model and weights are defined in rust/crates/core/src/embed.rs. Set search.embeddings.model to select another model. The API key comes from the platform credential store under search.embeddings.credential, or a process environment variable injected by a credential manager. Never save it in configuration, a dotenv file, the wiki, or logs.

Enabling embeddings sends wiki sections and queries to an external provider. Requests ask for zero data retention and no collection; that routing request is not an independent audit of the provider. The Windows tray can hand the key to the service through a restricted named pipe. The service retains the key in memory.

## Evaluation

Run node scripts/eval.mjs --set synthetic-hard --tier search for deterministic retrieval checks. scripts/embed-eval.mjs compares hosted models on the committed synthetic data and costs provider calls. Keep generated caches and reports in ignored storage.

Choose scoring settings on one dataset and evaluate them on a separate dataset. Record sample sizes, per-question changes and whether a result used the real provider. Synthetic tests do not establish performance on a private wiki.
