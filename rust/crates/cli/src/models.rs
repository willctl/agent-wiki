//! `agent-wiki models`: which models the curator and Ask use, which ones this ChatGPT account offers,
//! and changing them (aw_core::models). A change applies from their next run; no restart.
//!
//!   agent-wiki models [--json]                                   what each one uses, and the offer
//!   agent-wiki models set curator|ask <model> [--effort <level>] [--force]
//!   agent-wiki models set curator|ask --effort <level>           keeps the model
//!   agent-wiki models reset curator|ask                          back to the default

use aw_core::curator::CuratorCfg;
use aw_core::paths::{AppPaths, process_env};
use aw_core::{models, settings, wiki};
use serde_json::Value;

pub fn run(args: &[String]) -> ! {
    let env = process_env();
    let config = wiki::read_config(&env).unwrap_or_else(|e| fail(&e.to_string()));
    let (wiki_dir, _) = wiki::resolve_wiki_dir(&env).unwrap_or_else(|e| fail(&e.to_string()));
    // This runs as the person, so it can read Codex's list itself; the window gets it this way too.
    models::publish(&wiki_dir, &CuratorCfg::from_config(&config, &AppPaths::current()).codex_home);
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str);
    match args.first().map(String::as_str) {
        None | Some("--json") => {
            let o = models::overview(&wiki_dir, &config);
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&o).unwrap_or_default());
            } else {
                print!("{}", describe(&o));
            }
            std::process::exit(0);
        }
        Some(verb @ ("set" | "reset")) => {
            let role = args.get(1).map(String::as_str).unwrap_or_default();
            if !settings::MODEL_ROLES.contains(&role) {
                fail("say which one: curator or ask");
            }
            let (model, effort) = if verb == "reset" {
                (None, None)
            } else {
                let model = args.get(2).filter(|a| !a.starts_with("--")).cloned();
                let effort = flag("--effort").map(String::from);
                if model.is_none() && effort.is_none() {
                    fail("usage: agent-wiki models set curator|ask <model> [--effort <level>]");
                }
                // What is not given stays as chosen before.
                let keep = settings::model_choice(&wiki_dir, role);
                (model.or(keep.model), effort.or(keep.reasoning_effort))
            };
            let o = models::overview(&wiki_dir, &config);
            if !args.iter().any(|a| a == "--force")
                && let Some(why) = models::refusal(model.as_deref(), effort.as_deref(), o[role]["model"].as_str().unwrap_or_default(), Some(&o["available"]).filter(|a| a.is_object()))
            {
                fail(&format!("{why}. Use --force to save it anyway."));
            }
            match settings::set_model(&wiki_dir, role, model.as_deref(), effort.as_deref(), "cli") {
                Ok(_) => {
                    print!("{}", describe(&models::overview(&wiki_dir, &config)));
                    std::process::exit(0);
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        _ => fail("usage: agent-wiki models [--json] | models set curator|ask <model> [--effort <level>] [--force] | models reset curator|ask"),
    }
}

/// The overview as a few lines of text.
fn describe(o: &Value) -> String {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let line = |label: &str, role: &str| {
        let r = &o[role];
        let chosen = !r["choice"]["model"].is_null() || !r["choice"]["reasoningEffort"].is_null();
        let follows = role == "ask" && r["choice"]["model"].is_null() && o["defaults"]["ask"]["model"].is_null();
        format!("{label} {}, {} reasoning ({}{})\n", s(&r["model"]), s(&r["reasoningEffort"]), if chosen { "chosen" } else { "default" }, if follows { "; the curator's model" } else { "" })
    };
    let mut out = line("Curator:", "curator") + &line("Ask:    ", "ask");
    match o["available"]["models"].as_array() {
        Some(list) if !list.is_empty() => {
            out.push_str(&format!("\nThis ChatGPT account's models (Codex's list, {}):\n", s(&o["available"]["fetchedAt"])));
            for m in list.iter().filter(|m| m["listed"] == true) {
                let efforts: Vec<String> = m["efforts"].as_array().into_iter().flatten().map(s).collect();
                out.push_str(&format!("  {:<16} {:<44} {}\n", s(&m["slug"]), efforts.join(" "), s(&m["description"])));
            }
        }
        _ => out.push_str("\nThis ChatGPT account's model list is not known yet: it appears after the curator's first model run.\n"),
    }
    for p in o["problems"].as_array().into_iter().flatten() {
        out.push_str(&format!("\n! {}\n", s(p)));
    }
    out.push_str("\nChange: agent-wiki models set curator|ask <model> [--effort <level>]   (or the window: Status > Models)\n");
    out
}

fn fail(msg: &str) -> ! {
    eprintln!("agent-wiki models: {msg}");
    std::process::exit(1)
}
