//! Settings that belong to the wiki rather than to one program: .curator/settings.json, which the
//! window (through the service) and the curator both read: whether held changes and cleanups are
//! applied automatically (the default since 2026-10-06, with a notification and Undo) or wait for the
//! person ("Ask me first"), when scheduled cleanups run (daily at 03:00 by default, or any cron
//! schedule, or off), and which models the curator and Ask use (models.rs).

use crate::cron::Cron;
use crate::wiki::{self, Result, atomic_write, read_if_exists, wiki_err};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approvals {
    /// Apply held changes and cleanups right away, and notify.
    Auto,
    /// Wait for the person's OK.
    Manual,
}

impl Approvals {
    pub fn as_str(self) -> &'static str {
        match self {
            Approvals::Auto => "auto",
            Approvals::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Approvals> {
        match s {
            "auto" => Some(Approvals::Auto),
            "manual" => Some(Approvals::Manual),
            _ => None,
        }
    }
}

fn file(wiki_dir: &Path) -> PathBuf {
    wiki_dir.join(".curator").join("settings.json")
}

/// The file's JSON: Ok(None) when there is none, Err(why) when it is there but unusable.
fn read(wiki_dir: &Path) -> std::result::Result<Option<Value>, String> {
    let text = read_if_exists(&file(wiki_dir)).map_err(|e| format!("cannot read .curator/settings.json ({e})"))?;
    let Some(text) = text else { return Ok(None) };
    match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
        Ok(v @ Value::Object(_)) => Ok(Some(v)),
        Ok(_) => Err(".curator/settings.json is not a JSON object".into()),
        Err(e) => Err(format!(".curator/settings.json is not valid JSON ({e})")),
    }
}

/// The approval mode, and why it is not what the file says when the file is unusable. No file means
/// the default (automatic); a file that cannot be read or understood means "Ask me first", so a
/// damaged file never turns waiting off.
pub fn approvals_and_problem(wiki_dir: &Path) -> (Approvals, Option<String>) {
    match read(wiki_dir) {
        Ok(None) => (Approvals::Auto, None),
        Ok(Some(v)) => match &v["approvals"] {
            Value::Null => (Approvals::Auto, None),
            Value::String(s) if Approvals::parse(s).is_some() => (Approvals::parse(s).unwrap_or(Approvals::Manual), None),
            other => (Approvals::Manual, Some(format!(".curator/settings.json: approvals is {other}, not \"auto\" or \"manual\"; changes wait for your OK until it is fixed"))),
        },
        Err(why) => (Approvals::Manual, Some(format!("{why}; changes wait for your OK until it is fixed"))),
    }
}

pub fn approvals(wiki_dir: &Path) -> Approvals {
    approvals_and_problem(wiki_dir).0
}

/// Changes the settings and logs what changed (as `by`: "window" or "cli"), under the write lock (so a
/// curator batch sees the old or the new value, never a mix). Other settings in the file are kept; an
/// unusable file is replaced.
fn write_setting(wiki_dir: &Path, by: &str, what: &str, change: impl FnOnce(&mut Value)) -> Result<()> {
    wiki::with_lock(wiki_dir, || {
        let mut v = read(wiki_dir).ok().flatten().unwrap_or_else(|| json!({}));
        change(&mut v);
        std::fs::create_dir_all(wiki_dir.join(".curator"))?;
        atomic_write(&file(wiki_dir), &format!("{}\n", serde_json::to_string_pretty(&v).unwrap_or_default()), None, None)?;
        let now = crate::text::now();
        wiki::append_to_log(wiki_dir, &now, &format!("- {} · {by} · {what}", crate::text::local_hm(&now)), true)?;
        Ok(())
    })
}

/// Sets the mode and logs it.
pub fn set_approvals(wiki_dir: &Path, mode: &str) -> Result<Approvals> {
    let Some(m) = Approvals::parse(mode) else { return wiki_err("approvals: \"auto\" or \"manual\"") };
    let what = if m == Approvals::Auto { "Changes and cleanups are now applied automatically" } else { "Changes and cleanups now wait for your OK" };
    write_setting(wiki_dir, "window", what, |v| v["approvals"] = json!(m.as_str()))?;
    Ok(m)
}

/// When scheduled cleanups (lint.rs) run: on a cron schedule in local time, or never.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupSchedule {
    Off,
    Cron(Cron),
}

/// Daily at 03:00 local time. A computer asleep then runs the cleanup when the curator is next idle.
pub const DEFAULT_CLEANUP_SCHEDULE: &str = "0 3 * * *";

impl CleanupSchedule {
    /// "off", "hourly", "daily" (03:00), "weekly" (Mondays at 03:00), or a cron expression (cron.rs).
    pub fn parse(s: &str) -> std::result::Result<CleanupSchedule, String> {
        let expr = match s.trim().to_ascii_lowercase().as_str() {
            "off" | "never" => return Ok(CleanupSchedule::Off),
            "hourly" => "0 * * * *",
            "daily" => DEFAULT_CLEANUP_SCHEDULE,
            "weekly" => "0 3 * * 1",
            _ => s,
        };
        Cron::parse(expr).map(CleanupSchedule::Cron)
    }

    pub fn as_str(&self) -> &str {
        match self {
            CleanupSchedule::Off => "off",
            CleanupSchedule::Cron(c) => c.as_str(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            CleanupSchedule::Off => "Off".into(),
            CleanupSchedule::Cron(c) => c.describe(),
        }
    }

    pub fn cron(&self) -> Option<&Cron> {
        match self {
            CleanupSchedule::Off => None,
            CleanupSchedule::Cron(c) => Some(c),
        }
    }
}

fn default_schedule() -> CleanupSchedule {
    CleanupSchedule::parse(DEFAULT_CLEANUP_SCHEDULE).unwrap_or(CleanupSchedule::Off)
}

/// The cleanup schedule, and why it is not what the file says when the setting is unusable. A
/// setting that cannot be read means the default (a damaged file also means "Ask me first", so
/// those cleanups wait for the person; approvals_and_problem reports the file).
pub fn cleanup_schedule_and_problem(wiki_dir: &Path) -> (CleanupSchedule, Option<String>) {
    let Ok(Some(v)) = read(wiki_dir) else { return (default_schedule(), None) };
    match &v["cleanupSchedule"] {
        Value::Null => (default_schedule(), None),
        Value::String(s) => match CleanupSchedule::parse(s) {
            Ok(c) => (c, None),
            Err(why) => (default_schedule(), Some(format!(".curator/settings.json: cleanupSchedule \"{s}\" is not a schedule ({why}); cleanups run daily at 03:00 until it is fixed"))),
        },
        other => (default_schedule(), Some(format!(".curator/settings.json: cleanupSchedule is {other}, not a cron schedule or \"off\"; cleanups run daily at 03:00 until it is fixed"))),
    }
}

/// Sets the schedule and logs it.
pub fn set_cleanup_schedule(wiki_dir: &Path, schedule: &str) -> Result<CleanupSchedule> {
    let c = match CleanupSchedule::parse(schedule) {
        Ok(c) => c,
        Err(why) => return wiki_err(format!("cleanupSchedule: {why}")),
    };
    let what = match &c {
        CleanupSchedule::Off => "Scheduled cleanups are now off".to_string(),
        CleanupSchedule::Cron(cron) => format!("Cleanup schedule set: {} ({})", cron.describe(), cron.as_str()),
    };
    write_setting(wiki_dir, "window", &what, |v| v["cleanupSchedule"] = json!(c.as_str()))?;
    Ok(c)
}

/// The roles that run a model: the curator (filing notes, cleanups) and Ask (answering questions).
pub const MODEL_ROLES: [&str; 2] = ["curator", "ask"];

/// The model and reasoning effort chosen for a role in the window or with `agent-wiki models`
/// (.curator/settings.json "models"). Either may be missing: config.json or the defaults fill it in
/// (CuratorCfg::with_choice, AskCfg::with_choice).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelChoice {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

/// A model name as Codex takes it (`-m`): the file is untrusted, and only these characters pass.
pub fn valid_model(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.starts_with(|c: char| c.is_ascii_alphanumeric()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

/// A reasoning effort ("low", "xhigh"): it goes into a `-c` TOML value, so letters only.
pub fn valid_effort(s: &str) -> bool {
    (1..=16).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase())
}

pub fn model_choice(wiki_dir: &Path, role: &str) -> ModelChoice {
    let Ok(Some(v)) = read(wiki_dir) else { return ModelChoice::default() };
    let r = &v["models"][role];
    ModelChoice { model: r["model"].as_str().filter(|s| valid_model(s)).map(String::from), reasoning_effort: r["reasoningEffort"].as_str().filter(|s| valid_effort(s)).map(String::from) }
}

/// Chooses a role's model and reasoning effort (None: back to the default), and logs it.
pub fn set_model(wiki_dir: &Path, role: &str, model: Option<&str>, effort: Option<&str>, by: &str) -> Result<ModelChoice> {
    if !MODEL_ROLES.contains(&role) {
        return wiki_err(format!("role: \"curator\" or \"ask\", not \"{role}\""));
    }
    let model = model.map(str::trim).filter(|s| !s.is_empty());
    let effort = effort.map(str::trim).filter(|s| !s.is_empty());
    if let Some(m) = model.filter(|m| !valid_model(m)) {
        return wiki_err(format!("model: \"{m}\" is not a model name (letters, digits, . _ : -)"));
    }
    if let Some(e) = effort.filter(|e| !valid_effort(e)) {
        return wiki_err(format!("reasoningEffort: \"{e}\" is not a reasoning effort (low, medium, high, ...)"));
    }
    let choice = ModelChoice { model: model.map(String::from), reasoning_effort: effort.map(String::from) };
    let name = if role == "ask" { "Ask" } else { "Curator" };
    let what = match (model, effort) {
        (None, None) => format!("{name} model back to the default"),
        (Some(m), Some(e)) => format!("{name} model set: {m}, {e} reasoning"),
        (Some(m), None) => format!("{name} model set: {m}, default reasoning"),
        (None, Some(e)) => format!("{name} model: default model, {e} reasoning"),
    };
    write_setting(wiki_dir, by, &what, |v| {
        let mut r = serde_json::Map::new();
        if let Some(m) = model {
            r.insert("model".into(), json!(m));
        }
        if let Some(e) = effort {
            r.insert("reasoningEffort".into(), json!(e));
        }
        if !v["models"].is_object() {
            v["models"] = json!({});
        }
        if let Some(models) = v["models"].as_object_mut() {
            if r.is_empty() {
                models.remove(role);
            } else {
                models.insert(role.into(), Value::Object(r));
            }
        }
        if v["models"].as_object().is_some_and(|m| m.is_empty())
            && let Some(o) = v.as_object_mut()
        {
            o.remove("models");
        }
    })?;
    Ok(choice)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_default_to_auto_switch_and_fail_safe() {
        let w = std::env::temp_dir().join(format!("aw-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(w.join("log")).unwrap();
        assert_eq!(approvals_and_problem(&w), (Approvals::Auto, None));
        set_approvals(&w, "manual").unwrap();
        assert_eq!(approvals(&w), Approvals::Manual);
        set_approvals(&w, "auto").unwrap();
        assert_eq!(approvals(&w), Approvals::Auto);
        assert!(set_approvals(&w, "sometimes").is_err());
        // A damaged or hand-mistyped file means "Ask me first", with the reason for /status.
        for bad in ["{\"approvals\": \"manual\",}", "[]", "{\"approvals\": \"Manual\"}", "{\"approvals\": true}"] {
            std::fs::write(w.join(".curator").join("settings.json"), bad).unwrap();
            let (m, why) = approvals_and_problem(&w);
            assert_eq!(m, Approvals::Manual, "{bad}");
            assert!(why.is_some_and(|y| y.contains("settings.json")), "{bad}");
        }
        std::fs::write(w.join(".curator").join("settings.json"), "\u{feff}{\"approvals\": \"auto\"}").unwrap();
        assert_eq!(approvals_and_problem(&w), (Approvals::Auto, None), "a BOM is fine");
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn the_cleanup_schedule_defaults_to_daily_and_keeps_the_approvals() {
        let w = std::env::temp_dir().join(format!("aw-schedule-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(w.join("log")).unwrap();
        let (c, why) = cleanup_schedule_and_problem(&w);
        assert_eq!((c.as_str(), c.describe(), why), ("0 3 * * *", "Daily at 03:00".to_string(), None));
        set_approvals(&w, "manual").unwrap();
        assert_eq!(set_cleanup_schedule(&w, "weekly").unwrap().describe(), "Mondays at 03:00");
        assert_eq!(set_cleanup_schedule(&w, "30 18 * * 1-5").unwrap().as_str(), "30 18 * * 1-5");
        assert_eq!(approvals(&w), Approvals::Manual, "setting the schedule keeps the other settings");
        assert_eq!(set_cleanup_schedule(&w, "OFF").unwrap(), CleanupSchedule::Off);
        assert_eq!(cleanup_schedule_and_problem(&w), (CleanupSchedule::Off, None));
        let e = set_cleanup_schedule(&w, "0 25 * * *").unwrap_err().to_string();
        assert!(e.contains("hour: 25"), "{e}");
        assert_eq!(cleanup_schedule_and_problem(&w).0, CleanupSchedule::Off, "a refused schedule changes nothing");
        let log: String = walk(&w.join("log")).iter().map(|f| std::fs::read_to_string(f).unwrap()).collect();
        assert!(log.contains("Cleanup schedule set: Weekdays at 18:30 (30 18 * * 1-5)"), "{log}");
        assert!(log.contains("Scheduled cleanups are now off"), "{log}");
        // A mistyped schedule runs the default, and says so.
        std::fs::write(w.join(".curator").join("settings.json"), "{\"cleanupSchedule\": \"0 3 * *\"}").unwrap();
        let (c, why) = cleanup_schedule_and_problem(&w);
        assert_eq!(c.as_str(), "0 3 * * *");
        assert!(why.is_some_and(|y| y.contains("five fields")));
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn model_choices_are_kept_per_role_and_checked() {
        let w = std::env::temp_dir().join(format!("aw-model-choice-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(w.join("log")).unwrap();
        assert_eq!(model_choice(&w, "curator"), ModelChoice::default());
        set_cleanup_schedule(&w, "weekly").unwrap();
        set_model(&w, "curator", Some("gpt-6-astra"), Some("high"), "window").unwrap();
        set_model(&w, "ask", None, Some("medium"), "cli").unwrap();
        assert_eq!(model_choice(&w, "curator"), ModelChoice { model: Some("gpt-6-astra".into()), reasoning_effort: Some("high".into()) });
        assert_eq!(model_choice(&w, "ask"), ModelChoice { model: None, reasoning_effort: Some("medium".into()) });
        assert_eq!(cleanup_schedule_and_problem(&w).0.as_str(), "0 3 * * 1", "other settings are kept");
        for (m, e) in [(Some("x --flag"), None), (Some("a\"b"), None), (None, Some("low\"\nsandbox=1")), (None, Some("HIGH"))] {
            assert!(set_model(&w, "curator", m, e, "cli").is_err(), "{m:?} {e:?}");
        }
        assert!(set_model(&w, "editor", Some("gpt-6-astra"), None, "cli").is_err());
        set_model(&w, "curator", None, None, "window").unwrap();
        set_model(&w, "ask", None, None, "window").unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(w.join(".curator").join("settings.json")).unwrap()).unwrap();
        assert!(v.get("models").is_none(), "back to the defaults leaves no models entry: {v}");
        // A hand-edited file: what could reach Codex's command line is ignored.
        std::fs::write(w.join(".curator").join("settings.json"), r#"{"models": {"curator": {"model": "-c x=1", "reasoningEffort": "low\"x"}}}"#).unwrap();
        assert_eq!(model_choice(&w, "curator"), ModelChoice::default());
        let _ = std::fs::remove_dir_all(&w);
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
        out
    }
}
