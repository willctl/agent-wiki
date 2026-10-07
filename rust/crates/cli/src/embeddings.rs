//! `agent-wiki embeddings status|sync`: hybrid search's vectors from the command line (the curator
//! keeps them current by itself; see aw_core::embed).
//!
//!   agent-wiki embeddings status    the settings, whether the API key is reachable, how many vectors exist
//!   agent-wiki embeddings sync      embeds what has no vector yet and removes vectors whose text is gone

use aw_core::embed::{self, Settings};
use aw_core::paths::process_env;
use aw_core::wiki;
use serde_json::json;

pub fn run(args: &[String]) -> ! {
    let env = process_env();
    let config = wiki::read_config(&env).unwrap_or_else(|e| fail(&e.to_string()));
    let (wiki_dir, _) = wiki::resolve_wiki_dir(&env).unwrap_or_else(|e| fail(&e.to_string()));
    let Some(s) = Settings::from_config(&config) else {
        fail("Embeddings are off: set \"search\": {\"embeddings\": {\"enabled\": true}} in config.json (see docs/search-plan.md).");
    };
    let key = embed::api_key(&s);
    match args.first().map(String::as_str) {
        Some("status") => {
            let texts = aw_core::search::embedding_texts(&wiki_dir).unwrap_or_default();
            let have = texts.iter().filter(|t| embed::vector(&wiki_dir, &s.model, &embed::text_key(&s.model, t)).is_some()).count();
            println!("{}", json!({ "model": s.model, "weight": s.weight, "credential": s.credential, "keyAvailable": key.is_some(), "units": texts.len(), "embedded": have }));
            std::process::exit(0);
        }
        Some("sync") => {
            let Some(key) = key else { fail(&format!("No API key: set OPENROUTER_API_KEY, or store it in the credential store as \"{}\".", s.credential)) };
            match embed::sync(&wiki_dir, &s, &key) {
                Ok(r) => {
                    println!("{}", json!({ "model": s.model, "embedded": r.embedded, "kept": r.kept, "removed": r.removed }));
                    std::process::exit(0);
                }
                Err(e) => fail(&e),
            }
        }
        _ => fail("usage: agent-wiki embeddings status|sync"),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("agent-wiki embeddings: {msg}");
    std::process::exit(1)
}
