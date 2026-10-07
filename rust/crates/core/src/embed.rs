//! Embeddings for hybrid search (docs/search-plan.md, S2). Each search unit's text is embedded once by
//! a hosted model through OpenRouter and kept in the wiki's .curator/vectors/<model>/ (the service and
//! the curator both reach the wiki folder; Forget removes vectors whose text is gone). A search embeds
//! its query and fuses the semantic ranking with BM25 (search.rs). Everything here is off unless
//! config.json has `search.embeddings.enabled: true`, and every failure (no key, no network, a slow
//! answer) leaves search as plain BM25.
//!
//! The API key is never stored by Agent Wiki: it comes from OPENROUTER_API_KEY in the environment, or
//! from the platform's credential store under `search.embeddings.credential` (Windows Credential
//! Manager, a generic credential; the macOS keychain, a generic password with that service name).

use crate::text::hash_text;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

/// What config.json's `search.embeddings` asks for.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub model: String,
    /// The semantic side's weight in the convex combination (BM25 gets 1 - weight) for a query worded as
    /// a question, and for a few keywords: meaning helps most when the words are a person's, less
    /// when they were picked to match the page (chosen on the 77-question set, docs/search-plan.md).
    pub weight: f64,
    pub keyword_weight: f64,
    /// Where the API key lives in the platform's credential store.
    pub credential: String,
    /// A query embedding that takes longer than this is dropped and the search is BM25 alone.
    pub query_timeout: Duration,
    pub base_url: String,
}

impl Settings {
    /// From config.json; None when embeddings are not enabled.
    pub fn from_config(config: &Value) -> Option<Settings> {
        let e = &config["search"]["embeddings"];
        if e["enabled"].as_bool() != Some(true) {
            return None;
        }
        Some(Settings {
            model: e["model"].as_str().filter(|m| !m.is_empty()).unwrap_or("google/gemini-embedding-2").to_string(),
            weight: e["weight"].as_f64().filter(|w| (0.0..=1.0).contains(w)).unwrap_or(0.9),
            keyword_weight: e["keywordWeight"].as_f64().filter(|w| (0.0..=1.0).contains(w)).unwrap_or(0.5),
            credential: e["credential"].as_str().filter(|c| !c.is_empty()).unwrap_or("AgentWiki/OpenRouter").to_string(),
            query_timeout: Duration::from_millis(e["queryTimeoutMs"].as_u64().unwrap_or(1500).clamp(100, 30_000)),
            base_url: e["baseUrl"].as_str().filter(|u| u.starts_with("https://")).unwrap_or("https://openrouter.ai/api/v1").trim_end_matches('/').to_string(),
        })
    }
}

static SETTINGS: RwLock<Option<Settings>> = RwLock::new(None);

/// Sets this process's embedding settings (from config.json at startup). None turns hybrid search off.
pub fn configure(config: &Value) {
    *SETTINGS.write().unwrap_or_else(|e| e.into_inner()) = Settings::from_config(config);
}

pub fn settings() -> Option<Settings> {
    SETTINGS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

// ---------------------------------------------------------------- the key

/// The Windows service's pipe for the embeddings key handoff (cli/src/keypipe.rs, tray/src/keyhandoff.rs).
pub fn key_pipe_name(port: u16) -> String {
    format!(r"\\.\pipe\AgentWiki-embeddings-key-{port}")
}

static HANDED: Mutex<Option<String>> = Mutex::new(None);

/// A key handed to this process (the Windows service gets it from the tray); kept in memory only.
pub fn set_key(key: Option<String>) {
    *HANDED.lock().unwrap_or_else(|e| e.into_inner()) = key;
}

/// The API key, read when needed and never written anywhere by Agent Wiki. The credential store is
/// asked once per process when it has the key, at most once a minute when it does not (on macOS each
/// lookup starts 'security', which may ask the person to allow it).
pub fn api_key(s: &Settings) -> Option<String> {
    if let Some(k) = std::env::var("OPENROUTER_API_KEY").ok().filter(|k| !k.trim().is_empty()) {
        return Some(k.trim().to_string());
    }
    if let Some(k) = HANDED.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(k);
    }
    type Looked = Option<(String, i64, Option<String>)>;
    static LOOKED: Mutex<Looked> = Mutex::new(None);
    let mut looked = LOOKED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((target, at, key)) = looked.as_ref()
        && *target == s.credential
        && (key.is_some() || crate::text::now_ms() - at < 60_000)
    {
        return key.clone();
    }
    let key = platform_secret(&s.credential);
    *looked = Some((s.credential.clone(), crate::text::now_ms(), key.clone()));
    key
}

#[cfg(windows)]
fn platform_secret(target: &str) -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredFree, CredReadW};
    let wide: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: CredReadW fills `cred` on success; it is read once and freed with CredFree.
    unsafe {
        if CredReadW(wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) == 0 || cred.is_null() {
            return None;
        }
        let c = &*cred;
        let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
        // Generic credentials written by most tools hold UTF-16LE text; fall back to UTF-8.
        let text = if bytes.len() % 2 == 0 && bytes.len() >= 2 && bytes[1] == 0 {
            String::from_utf16(&bytes.as_chunks::<2>().0.iter().map(|p| u16::from_le_bytes(*p)).collect::<Vec<_>>()).ok()
        } else {
            String::from_utf8(bytes.to_vec()).ok()
        };
        CredFree(cred as *const _);
        text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
    }
}

#[cfg(target_os = "macos")]
fn platform_secret(target: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/security").args(["find-generic-password", "-s", target, "-w"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|k| !k.is_empty())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_secret(target: &str) -> Option<String> {
    let out = std::process::Command::new("secret-tool").args(["lookup", "service", target]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|k| !k.is_empty())
}

// ---------------------------------------------------------------- the API

/// The system's curl (Windows 10 and later, macOS, and nearly every Linux): it uses the platform's TLS
/// and trust store, so a certificate the machine trusts (a company proxy's) is trusted here too, and it
/// keeps a TLS stack out of agent-wiki itself, which every hook run would otherwise load (measured
/// 2026-10-06: crypt32 and secur32 imports took a 15 ms start to 60 ms).
fn curl() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join("System32").join("curl.exe")
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/usr/bin/curl")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        PathBuf::from(if Path::new("/usr/bin/curl").exists() { "/usr/bin/curl" } else { "curl" })
    }
}

/// How a model wants a query worded (its model card); passages go in as they are.
fn query_form(model: &str, q: &str) -> String {
    if model.contains("qwen3-embedding") {
        format!("Instruct: Given a question about a personal wiki, retrieve the passages that answer it\nQuery: {q}")
    } else if model.contains("e5-") || model.contains("nemotron") {
        format!("query: {q}")
    } else if model.contains("bge-") && model.contains("-en") {
        format!("Represent this sentence for searching relevant passages: {q}")
    } else {
        q.to_string()
    }
}

fn passage_form(model: &str, p: &str) -> String {
    if model.contains("e5-") || model.contains("nemotron") { format!("passage: {p}") } else { p.to_string() }
}

/// One embeddings request: Ok(vectors), or Err((status, message)). It asks OpenRouter for providers
/// that neither collect nor retain the data. The key reaches curl on stdin, as a config line, never in
/// its arguments or a file; the private request body uses the same pipe. Errors never name the key.
fn request_config(key: &str, body: &Value) -> Result<String, (u16, String)> {
    if key.is_empty() || key.len() > 4096 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)) {
        return Err((0, "embedding credential is not a valid bearer token".into()));
    }
    // curl's quoted config strings recognize backslash and quote escapes. JSON already represents
    // control characters as escapes; double those backslashes so curl sends the exact JSON bytes.
    let escaped = body.to_string().replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("header = \"Authorization: Bearer {key}\"\ndata-binary = \"{escaped}\"\n"))
}

fn request(s: &Settings, key: &str, input: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>, (u16, String)> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let body = json!({ "model": s.model, "input": input, "provider": { "data_collection": "deny", "zdr": true } });
    let config = request_config(key, &body)?;
    let mut cmd = Command::new(curl());
    // -q must be first: unrelated user curl settings must not redirect or persist private requests.
    cmd.args(["-q", "-sS", "--max-time", &format!("{:.1}", timeout.as_secs_f64()), "-H", "Content-Type: application/json", "-H", "X-Title: Agent Wiki", "-A"])
        .arg(format!("agent-wiki/{}", crate::VERSION))
        .args(["-w", "\n%{http_code}", "-K", "-"])
        .arg(format!("{}/embeddings", s.base_url))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.spawn().and_then(|mut child| {
        if let Some(mut stdin) = child.stdin.take()
            && let Err(e) = stdin.write_all(config.as_bytes())
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        child.wait_with_output()
    });
    let out = out.map_err(|e| (0, format!("could not run curl ({}): {e}", curl().display())))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (json_text, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    let status: u16 = code.trim().parse().unwrap_or(0);
    if status == 0 {
        return Err((0, format!("embedding request failed: {}", short(&String::from_utf8_lossy(&out.stderr)))));
    }
    let v: Value = serde_json::from_str(json_text).unwrap_or(Value::Null);
    let data = v["data"]
        .as_array()
        .filter(|d| status == 200 && d.len() == input.len())
        .ok_or_else(|| (status, format!("embedding API answered {status}: {}", short(&v["error"]["message"].as_str().map(String::from).unwrap_or_else(|| json_text.to_string())))))?;
    let mut vecs = Vec::with_capacity(data.len());
    for d in data {
        let raw: Vec<f32> = d["embedding"].as_array().map(|a| a.iter().filter_map(Value::as_f64).map(|x| x as f32).collect()).unwrap_or_default();
        let norm = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        if raw.is_empty() || norm == 0.0 {
            return Err((status, "embedding API returned an empty vector".into()));
        }
        vecs.push(raw.into_iter().map(|x| x / norm).collect());
    }
    Ok(vecs)
}

/// One text's vector, filled in by whichever worker requested it.
type Slot = Mutex<Option<Result<Vec<f32>, String>>>;

/// Embeds texts, each as a unit vector: 32 per request, or, when the model's zero-retention
/// providers take only one input per request (gemini-embedding-2 on OpenRouter: batches go to a
/// provider that retains data), one per request, six at a time.
pub fn embed(s: &Settings, key: &str, texts: &[String], timeout: Duration) -> Result<Vec<Vec<f32>>, String> {
    static SINGLES: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let singles = || SINGLES.lock().unwrap_or_else(|e| e.into_inner()).contains(&s.model);
    let mut out = Vec::with_capacity(texts.len());
    let mut rest = texts;
    while !rest.is_empty() && !singles() && texts.len() > 1 {
        let n = rest.len().min(32);
        match request(s, key, &rest[..n], timeout) {
            Ok(v) => {
                out.extend(v);
                rest = &rest[n..];
            }
            Err((404, m)) if m.contains("data policy") => SINGLES.lock().unwrap_or_else(|e| e.into_inner()).push(s.model.clone()),
            Err((_, m)) => return Err(m),
        }
    }
    if rest.is_empty() {
        return Ok(out);
    }
    let results: Vec<Slot> = rest.iter().map(|_| Mutex::new(None)).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..rest.len().min(6) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if i >= rest.len() {
                        break;
                    }
                    let r = request(s, key, std::slice::from_ref(&rest[i]), timeout).map(|mut v| v.remove(0)).map_err(|(_, m)| m);
                    let failed = r.is_err();
                    *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                    if failed {
                        next.store(rest.len(), std::sync::atomic::Ordering::SeqCst);
                    }
                }
            });
        }
    });
    for r in results {
        match r.into_inner().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(v)) => out.push(v),
            Some(Err(m)) => return Err(m),
            None => return Err("embedding stopped after an error".into()),
        }
    }
    Ok(out)
}

fn short(s: &str) -> String {
    let s = crate::text::one_line(s);
    if s.chars().count() > 200 { format!("{}…", s.chars().take(200).collect::<String>()) } else { s }
}

// ---------------------------------------------------------------- vectors on disk

fn model_dir(wiki_dir: &Path, model: &str) -> PathBuf {
    let safe: String = model.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    wiki_dir.join(".curator").join("vectors").join(safe)
}

/// The key of a unit's text under a model (the file name of its vector).
pub fn text_key(model: &str, text: &str) -> String {
    hash_text(&format!("{model}\n{text}"), 32)
}

fn vector_file(dir: &Path, key: &str) -> PathBuf {
    dir.join(&key[..2]).join(format!("{key}.f32"))
}

type Cache = Mutex<HashMap<PathBuf, Arc<Vec<f32>>>>;

fn cache() -> &'static Cache {
    static C: OnceLock<Cache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// A unit's vector, if it has been embedded (read once per process; vectors never change for a key).
pub fn vector(wiki_dir: &Path, model: &str, key: &str) -> Option<Arc<Vec<f32>>> {
    let file = vector_file(&model_dir(wiki_dir, model), key);
    if let Some(v) = cache().lock().unwrap_or_else(|e| e.into_inner()).get(&file) {
        return Some(v.clone());
    }
    let bytes = fs::read(&file).ok()?;
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    let v: Arc<Vec<f32>> = Arc::new(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(file, v.clone());
    Some(v)
}

/// Stores a unit's vector (sync does this; tests use it to set up vectors without the API).
pub fn put(wiki_dir: &Path, model: &str, key: &str, v: &[f32]) -> std::io::Result<()> {
    store(&model_dir(wiki_dir, model), key, v)
}

fn store(dir: &Path, key: &str, v: &[f32]) -> std::io::Result<()> {
    let file = vector_file(dir, key);
    fs::create_dir_all(file.parent().unwrap_or(dir))?;
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, &file)
}

/// What sync did.
#[derive(Debug, Default, PartialEq)]
pub struct Synced {
    pub embedded: usize,
    pub kept: usize,
    pub removed: usize,
}

/// Brings the vectors in line with the wiki: embeds every unit text that has none, and removes vectors
/// whose text is gone (an edited section, a forgotten text). Run by the curator, which can read the key.
pub fn sync(wiki_dir: &Path, s: &Settings, key: &str) -> Result<Synced, String> {
    let dir = model_dir(wiki_dir, &s.model);
    let texts = crate::search::embedding_texts(wiki_dir).map_err(|e| e.to_string())?;
    let mut wanted: HashMap<String, String> = HashMap::new();
    for t in texts {
        wanted.entry(text_key(&s.model, &t)).or_insert(t);
    }
    let missing: Vec<(&String, &String)> = wanted.iter().filter(|(k, _)| !vector_file(&dir, k).exists()).collect();
    let mut done = Synced { kept: wanted.len() - missing.len(), ..Default::default() };
    for chunk in missing.chunks(32) {
        let inputs: Vec<String> = chunk.iter().map(|(_, t)| passage_form(&s.model, t)).collect();
        let vecs = embed(s, key, &inputs, Duration::from_secs(120))?;
        for ((k, _), v) in chunk.iter().zip(vecs) {
            store(&dir, k, &v).map_err(|e| format!("could not store a vector: {e}"))?;
            done.embedded += 1;
        }
    }
    for shard in fs::read_dir(&dir).into_iter().flatten().flatten() {
        for f in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            if name.strip_suffix(".f32").is_some_and(|k| !wanted.contains_key(k)) || name.contains(".tmp") {
                let _ = fs::remove_file(f.path());
                done.removed += 1;
            }
        }
    }
    Ok(done)
}

/// The query's vector, or None (embeddings off, no key, no vectors yet, or no answer in time).
pub fn query_vector(wiki_dir: &Path, query: &str) -> Option<(Settings, Vec<f32>)> {
    let s = settings()?;
    if !model_dir(wiki_dir, &s.model).is_dir() {
        return None;
    }
    static RECENT: OnceLock<Mutex<HashMap<String, Vec<f32>>>> = OnceLock::new();
    let recent = RECENT.get_or_init(Default::default);
    let k = text_key(&s.model, query);
    if let Some(v) = recent.lock().unwrap_or_else(|e| e.into_inner()).get(&k) {
        return Some((s, v.clone()));
    }
    let key = api_key(&s)?;
    let v = embed(&s, &key, &[query_form(&s.model, query)], s.query_timeout).ok()?.into_iter().next()?;
    let mut r = recent.lock().unwrap_or_else(|e| e.into_inner());
    if r.len() > 256 {
        r.clear();
    }
    r.insert(k, v.clone());
    Some((s, v))
}

/// For the curator's idle loop: sync at most once a minute, when embeddings are on and the key is
/// reachable. Returns what happened, or None when it did not run.
pub fn sync_if_due(wiki_dir: &Path) -> Option<Result<Synced, String>> {
    static LAST: Mutex<i64> = Mutex::new(0);
    let s = settings()?;
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if crate::text::now_ms() - *last < 60_000 {
            return None;
        }
        *last = crate::text::now_ms();
    }
    let key = api_key(&s)?;
    Some(sync(wiki_dir, &s, &key))
}

/// Whether a query reads as a person's question (a question mark, a question word first, or seven
/// words or more) rather than a few search keywords.
pub fn question_like(query: &str) -> bool {
    const ASK: [&str; 16] = ["what", "who", "whom", "whose", "when", "where", "which", "why", "how", "did", "do", "does", "is", "are", "can", "should"];
    let words: Vec<String> = query.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).filter(|w| !w.is_empty()).collect();
    query.trim_end().ends_with('?') || words.first().is_some_and(|w| ASK.contains(&w.as_str())) || words.len() >= 7
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_request_sends_private_json_only_through_stdin() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                    Err(e) => panic!("local curl fixture did not receive a request: {e}"),
                }
            };
            // Accepted sockets inherit the listener's nonblocking mode on Windows.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
                headers.push_str(&line);
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).unwrap();
            let response = r#"{"data":[{"embedding":[3,4]}]}"#;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            (headers, serde_json::from_slice::<Value>(&body).unwrap())
        });
        let mut settings = Settings::from_config(&json!({"search":{"embeddings":{"enabled":true}}})).unwrap();
        settings.base_url = format!("http://127.0.0.1:{port}"); // synthetic loopback fixture, never a provider
        let input = vec!["Synthetic private passage: \"quoted\", C:\\fixture, Unicode café, newline\nsecond line and tab\tend".into()];
        let answer = request(&settings, "audit-synthetic-token", &input, Duration::from_secs(5));
        let (headers, body) = server.join().unwrap();
        assert_eq!(answer.unwrap(), vec![vec![0.6, 0.8]]);
        assert!(headers.contains("Authorization: Bearer audit-synthetic-token\r\n"));
        assert_eq!(body["input"], json!(input));
        assert_eq!(body["provider"], json!({"data_collection":"deny","zdr":true}));
        for key in ["", "bad\r\nheader", "bad\"\nurl = other", "bad\\token"] {
            assert!(request_config(key, &body).is_err(), "invalid credential accepted");
        }
    }

    #[test]
    fn settings_need_enabled_and_clamp() {
        assert_eq!(Settings::from_config(&json!({})), None);
        assert_eq!(Settings::from_config(&json!({ "search": { "embeddings": { "enabled": false } } })), None);
        let s = Settings::from_config(&json!({ "search": { "embeddings": { "enabled": true, "weight": 3, "queryTimeoutMs": 5, "baseUrl": "http://plain.example" } } })).unwrap();
        assert_eq!((s.model.as_str(), s.weight, s.keyword_weight, s.query_timeout.as_millis(), s.base_url.as_str()), ("google/gemini-embedding-2", 0.9, 0.5, 100, "https://openrouter.ai/api/v1"));
        assert_eq!(query_form("intfloat/e5-large-v2", "x"), "query: x");
        assert!(question_like("Where am I staying in Portugal") && question_like("lisbon hotel?") && question_like("the port the harbor sync uses after the change"));
        assert!(!question_like("Lisbon hotel") && !question_like("restore drill photos task excluded"));
        assert_eq!(query_form("google/gemini-embedding-2", "x"), "x");
    }

    #[test]
    fn vectors_round_trip() {
        let w = std::env::temp_dir().join(format!("aw-embed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&w);
        let dir = model_dir(&w, "m/x");
        let k = text_key("m/x", "hello");
        store(&dir, &k, &[0.6, 0.8]).unwrap();
        assert_eq!(*vector(&w, "m/x", &k).unwrap(), vec![0.6, 0.8]);
        assert!(dir.ends_with("m_x"));
        let _ = fs::remove_dir_all(&w);
    }
}
