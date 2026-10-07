//! `agent-wiki forget "<text>" [--ignore-case] [--yes]`: lists every copy of a piece of text in the
//! wiki and the logs, and with --yes redacts them all (see aw_core::forget).

use aw_core::paths::AppPaths;
use aw_core::wiki;

pub fn run(args: &[String]) -> ! {
    let text = args.iter().find(|a| !a.starts_with("--")).cloned().unwrap_or_default();
    let ignore_case = args.iter().any(|a| a == "--ignore-case");
    let yes = args.iter().any(|a| a == "--yes");
    if text.trim().is_empty() {
        eprintln!("usage: agent-wiki forget \"<text>\" [--ignore-case] [--yes]");
        std::process::exit(2);
    }
    let env = crate::mcp::env();
    let wiki_dir = match wiki::resolve_wiki_dir(&env) {
        Ok((d, _)) => d,
        Err(e) => {
            eprintln!("agent-wiki forget: {e}");
            std::process::exit(1);
        }
    };
    let logs = AppPaths::current().log_dir;
    let found = match aw_core::forget::find(&wiki_dir, Some(&logs), &text, ignore_case) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("agent-wiki forget: {e}");
            std::process::exit(2);
        }
    };
    if found.is_empty() {
        println!("Not found anywhere in {} or {}.", wiki::disp(&wiki_dir), wiki::disp(&logs));
        std::process::exit(0);
    }
    let total: usize = found.iter().map(|f| f.count).sum();
    println!("{total} match(es) in {} file(s):", found.len());
    for f in &found {
        println!("  {:>3}  {}  {}", f.count, f.rel, f.sample);
    }
    if !yes {
        println!("\nNothing changed. Run again with --yes to redact them all.");
        std::process::exit(0);
    }
    match aw_core::forget::redact(&wiki_dir, Some(&logs), &text, ignore_case, "cli") {
        Ok(r) => {
            println!("\nRedacted {} match(es) in {} file(s).", r["matches"], r["files"]);
            for f in r["failed"].as_array().into_iter().flatten() {
                println!("  could not change {}", f.as_str().unwrap_or(""));
            }
            println!("A git history of the wiki, if you keep one, still has the old text.");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("agent-wiki forget: {e}");
            std::process::exit(1)
        }
    }
}
