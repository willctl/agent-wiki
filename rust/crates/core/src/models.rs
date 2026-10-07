//! Which models the curator and Ask use, and which ones this ChatGPT account offers.
//!
//! The choice: the window (or `agent-wiki models`) writes .curator/settings.json "models"
//! (settings::set_model); what it leaves out comes from config.json, then the defaults (the curator:
//! gpt-6.1-sol with medium reasoning; Ask: the curator's model with low reasoning). The curator and
//! Ask read it before each run, so a change needs no restart.
//!
//! The offer: Codex keeps the account's model list in models_cache.json in its CODEX_HOME (the
//! curator's own one). The curator, which runs as the person, copies the useful part of it into the
//! wiki (.curator/models.json), so the service (another account on Windows) can show it and check a
//! choice against it.

use crate::askworker::AskCfg;
use crate::curator::CuratorCfg;
use crate::wiki::{atomic_write, read_if_exists};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn published_file(wiki_dir: &Path) -> PathBuf {
    wiki_dir.join(".curator").join("models.json")
}

/// Codex's model list, trimmed to what the window needs, most useful first: slug, display name,
/// description, default and supported reasoning efforts, and whether Codex lists it in its picker.
pub fn from_codex_cache(codex_home: &Path) -> Option<Value> {
    let text = read_if_exists(&codex_home.join("models_cache.json")).ok().flatten()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut models: Vec<(i64, Value)> = v["models"]
        .as_array()?
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str().filter(|s| crate::settings::valid_model(s))?;
            let efforts: Vec<&str> =
                m["supported_reasoning_levels"].as_array().into_iter().flatten().filter_map(|l| l["effort"].as_str().or(l.as_str())).filter(|e| crate::settings::valid_effort(e)).collect();
            Some((
                m["priority"].as_i64().unwrap_or(i64::MAX),
                json!({
                    "slug": slug,
                    "name": m["display_name"].as_str().unwrap_or(slug),
                    "description": m["description"].as_str().unwrap_or(""),
                    "defaultEffort": m["default_reasoning_level"].as_str().filter(|e| crate::settings::valid_effort(e)),
                    "efforts": efforts,
                    "listed": m["visibility"].as_str() == Some("list"),
                }),
            ))
        })
        .collect();
    models.sort_by_key(|(p, m)| (!m["listed"].as_bool().unwrap_or(false), *p));
    Some(json!({ "fetchedAt": v["fetched_at"], "models": models.into_iter().map(|(_, m)| m).collect::<Vec<_>>() }))
}

/// Copies the account's model list into the wiki when it changed. Returns whether it wrote.
pub fn publish(wiki_dir: &Path, codex_home: &Path) -> bool {
    let Some(list) = from_codex_cache(codex_home) else { return false };
    let text = format!("{}\n", serde_json::to_string_pretty(&list).unwrap_or_default());
    if read_if_exists(&published_file(wiki_dir)).ok().flatten().as_deref() == Some(text.as_str()) {
        return false;
    }
    let _ = std::fs::create_dir_all(wiki_dir.join(".curator"));
    atomic_write(&published_file(wiki_dir), &text, None, None).is_ok()
}

/// The list the curator published, re-checked (the wiki folder is untrusted): None until it has.
pub fn published(wiki_dir: &Path) -> Option<Value> {
    let v: Value = serde_json::from_str(&read_if_exists(&published_file(wiki_dir)).ok().flatten()?).ok()?;
    let models: Vec<Value> = v["models"]
        .as_array()?
        .iter()
        .filter(|m| m["slug"].as_str().is_some_and(crate::settings::valid_model))
        .map(|m| {
            json!({
                "slug": m["slug"],
                "name": m["name"].as_str().unwrap_or_default(),
                "description": m["description"].as_str().unwrap_or_default(),
                "defaultEffort": m["defaultEffort"].as_str().filter(|e| crate::settings::valid_effort(e)),
                "efforts": m["efforts"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|e| crate::settings::valid_effort(e)).collect::<Vec<_>>(),
                "listed": m["listed"].as_bool().unwrap_or(false),
            })
        })
        .collect();
    Some(json!({ "fetchedAt": v["fetchedAt"].as_str(), "models": models }))
}

/// What is wrong with a role's model for this account, in words: not in its list, or a reasoning
/// effort the model does not offer. Nothing when the list is not known yet.
pub fn problems(who: &str, model: &str, effort: &str, available: Option<&Value>) -> Vec<String> {
    let Some(list) = available.and_then(|a| a["models"].as_array()).filter(|l| !l.is_empty()) else { return vec![] };
    let Some(m) = list.iter().find(|m| m["slug"] == model) else {
        return vec![format!("{who}'s model {model} is not in this ChatGPT account's model list; choose another (window > Status > Models)")];
    };
    let efforts: Vec<&str> = m["efforts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !efforts.is_empty() && !efforts.contains(&effort) {
        return vec![format!("{who}'s model {model} does not offer {effort} reasoning (it offers {}); choose another (window > Status > Models)", efforts.join(", "))];
    }
    vec![]
}

/// Why a new choice for a role should not be saved, checked against the account's list when it is
/// known: a model it does not offer, or a reasoning effort the model (the new one, or the one the
/// role keeps) does not offer. None: fine.
pub fn refusal(model: Option<&str>, effort: Option<&str>, current_model: &str, available: Option<&Value>) -> Option<String> {
    let list = available.and_then(|a| a["models"].as_array()).filter(|l| !l.is_empty())?;
    let slug = model.unwrap_or(current_model);
    let Some(m) = list.iter().find(|m| m["slug"] == slug) else {
        let offered: Vec<&str> = list.iter().filter(|m| m["listed"] == true).filter_map(|m| m["slug"].as_str()).collect();
        return model.map(|m| format!("{m} is not in this ChatGPT account's model list ({})", offered.join(", ")));
    };
    let efforts: Vec<&str> = m["efforts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    effort.filter(|e| !efforts.is_empty() && !efforts.contains(e)).map(|e| format!("{slug} does not offer {e} reasoning (it offers {})", efforts.join(", ")))
}

/// The models as /status and the window show them: each role's model and reasoning effort in use,
/// what was chosen (`choice`; empty fields are the defaults), the defaults, the account's list, and
/// what is wrong.
pub fn overview(wiki_dir: &Path, config: &Value) -> Value {
    let curator = CuratorCfg::from_config(config, &crate::paths::AppPaths::current());
    let ask = AskCfg::from_config(config, &curator);
    let (c, a) = (curator.with_choice(wiki_dir), ask.with_choice(wiki_dir));
    let available = published(wiki_dir);
    let mut wrong = problems("the curator", &c.model, &c.reasoning_effort, available.as_ref());
    wrong.extend(problems("Ask", &a.model, &a.reasoning_effort, available.as_ref()));
    let role = |model: &str, effort: &str, role: &str| {
        let choice = crate::settings::model_choice(wiki_dir, role);
        json!({ "model": model, "reasoningEffort": effort, "choice": { "model": choice.model, "reasoningEffort": choice.reasoning_effort } })
    };
    json!({
        "curator": role(&c.model, &c.reasoning_effort, "curator"),
        "ask": role(&a.model, &a.reasoning_effort, "ask"),
        "defaults": {
            "curator": { "model": curator.model, "reasoningEffort": curator.reasoning_effort },
            // null: Ask uses the curator's model.
            "ask": { "model": if ask.follows_curator { Value::Null } else { json!(ask.model) }, "reasoningEffort": ask.reasoning_effort },
        },
        "available": available,
        "problems": wrong,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_list_is_trimmed_published_and_checked() {
        let w = std::env::temp_dir().join(format!("aw-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        let home = w.join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(w.join("wiki")).unwrap();
        let cache = json!({
            "fetched_at": "2026-10-06T20:20:25Z", "etag": "x", "identity": { "email": "someone@example.test" },
            "models": [
                { "slug": "hidden-one", "display_name": "Hidden", "visibility": "hide", "priority": 1, "default_reasoning_level": "medium", "supported_reasoning_levels": [{ "effort": "low" }] },
                { "slug": "gpt-b", "display_name": "GPT-B", "visibility": "list", "priority": 2, "default_reasoning_level": "medium", "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "medium" }] },
                { "slug": "gpt-a", "display_name": "GPT-A", "description": "Workhorse", "visibility": "list", "priority": 1, "default_reasoning_level": "low", "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "medium" }, { "effort": "high" }] },
                { "slug": "bad slug\"", "visibility": "list", "priority": 0 },
            ]
        });
        std::fs::write(home.join("models_cache.json"), cache.to_string()).unwrap();
        let wiki = w.join("wiki");
        assert!(publish(&wiki, &home));
        assert!(!publish(&wiki, &home), "unchanged: not written again");
        let text = std::fs::read_to_string(wiki.join(".curator").join("models.json")).unwrap();
        assert!(!text.contains("someone@example.test"), "the account identity stays out of the wiki");
        let list = published(&wiki).unwrap();
        let slugs: Vec<&str> = list["models"].as_array().unwrap().iter().filter_map(|m| m["slug"].as_str()).collect();
        assert_eq!(slugs, ["gpt-a", "gpt-b", "hidden-one"], "listed first, by priority; a bad slug is dropped");
        assert_eq!(list["models"][0]["efforts"], json!(["low", "medium", "high"]));
        assert!(problems("the curator", "gpt-a", "high", Some(&list)).is_empty());
        assert!(problems("the curator", "gpt-z", "low", Some(&list))[0].contains("not in this ChatGPT account's model list"));
        assert!(problems("Ask", "gpt-b", "high", Some(&list))[0].contains("does not offer high reasoning (it offers low, medium)"));
        assert!(problems("Ask", "anything", "low", None).is_empty(), "no list yet: nothing to check against");
        assert_eq!(refusal(Some("gpt-a"), Some("high"), "gpt-b", Some(&list)), None);
        assert!(refusal(Some("gpt-z"), None, "gpt-a", Some(&list)).unwrap().contains("not in this ChatGPT account's model list (gpt-a, gpt-b)"));
        assert!(refusal(None, Some("high"), "gpt-b", Some(&list)).unwrap().contains("gpt-b does not offer high reasoning"), "an effort alone is checked against the model the role keeps");
        assert_eq!(refusal(Some("gpt-z"), None, "gpt-a", None), None, "no list: any name");
        let _ = std::fs::remove_dir_all(&w);
    }
}
