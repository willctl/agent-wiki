//! The curator, hosted as the user: `agent-wiki curator --parent-stdin`, because the curator uses the
//! user's own Codex sign-in, which the service account must never have. Restarted if it exits (with
//! backoff) and when agent-wiki is replaced on disk (an upgrade needs no re-login). Its output goes to
//! logs/curator.log, with the host's own lines marked [tray].

use crate::config::Config;
use aw_core::sys::file_identity;
use aw_core::waker::Waker;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_LOG: u64 = 5 * 1024 * 1024;

pub struct CuratorHost {
    cfg: Config,
    waker: Arc<Waker>,
    restart: AtomicBool,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl CuratorHost {
    pub fn start(cfg: &Config) -> Arc<CuratorHost> {
        let host = Arc::new(CuratorHost { cfg: cfg.clone(), waker: Waker::new(), restart: AtomicBool::new(false), thread: Mutex::new(None) });
        let me = host.clone();
        *host.thread.lock().unwrap() = Some(std::thread::spawn(move || me.run()));
        host
    }

    pub fn restart(&self) {
        self.restart.store(true, Ordering::SeqCst);
        self.waker.wake();
    }

    /// Stops the curator (it finishes a commit in progress) and waits for the host to end.
    pub fn stop(&self) {
        self.waker.stop();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let deadline = Instant::now() + Duration::from_secs(25);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }

    fn log(&self, msg: &str) {
        crate::log::append(&self.cfg.log_dir, "curator.log", MAX_LOG, msg);
    }

    fn spawn(&self) -> std::io::Result<Child> {
        let mut cmd = Command::new(&self.cfg.agent);
        cmd.args(["curator", "--parent-stdin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(dir) = self.cfg.agent.parent() {
            cmd.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        cmd.spawn()
    }

    fn run(&self) {
        let mut backoff = 1000u64;
        while !self.waker.stopped() {
            self.restart.store(false, Ordering::SeqCst);
            let stamp = file_identity(&self.cfg.agent);
            let started = Instant::now();
            let mut child = match self.spawn() {
                Ok(c) => c,
                Err(e) => {
                    self.log(&format!("[tray] cannot start the curator ({}): {e}", self.cfg.agent.display()));
                    if self.waker.wait_stop(60_000) {
                        break;
                    }
                    continue;
                }
            };
            self.log(&format!("[tray] started curator pid {}", child.id()));
            let readers: Vec<_> = [child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>)]
                .into_iter()
                .flatten()
                .map(|r| {
                    let dir = self.cfg.log_dir.clone();
                    std::thread::spawn(move || {
                        for line in BufReader::new(r).lines().map_while(Result::ok) {
                            crate::log::append(&dir, "curator.log", MAX_LOG, &line);
                        }
                    })
                })
                .collect();
            let mut changed = false;
            loop {
                if matches!(child.try_wait(), Ok(Some(_))) || self.waker.stopped() || self.restart.load(Ordering::SeqCst) {
                    break;
                }
                self.waker.nap(1000);
                let now = file_identity(&self.cfg.agent);
                if now.is_some() && now != stamp {
                    changed = true;
                    self.log("[tray] agent-wiki changed on disk; restarting the curator");
                    break;
                }
            }
            if matches!(child.try_wait(), Ok(None)) {
                // Closing stdin asks the curator to stop: it finishes a commit in progress, or abandons a model call.
                drop(child.stdin.take());
                let deadline = Instant::now() + Duration::from_secs(15);
                while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if matches!(child.try_wait(), Ok(None)) {
                    self.log(&format!("[tray] curator did not stop within 15 s; killing pid {}", child.id()));
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            let code = child.wait().ok().and_then(|s| s.code()).map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            for r in readers {
                let _ = r.join();
            }
            let lived = started.elapsed().as_secs();
            if self.waker.stopped() {
                break;
            }
            if changed || self.restart.load(Ordering::SeqCst) {
                backoff = 1000;
                continue;
            }
            self.log(&format!("[tray] curator exited with code {code} after {lived}s; restarting in {backoff}ms"));
            if self.waker.wait_stop(backoff as i64) {
                break;
            }
            backoff = if lived > 60 { 1000 } else { (backoff * 2).min(60_000) };
        }
        self.log("[tray] curator host stopped");
    }
}
