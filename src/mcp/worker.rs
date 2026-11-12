//! Tool-call workers: every `tools/call` gets its own thread, so the
//! protocol core NEVER blocks behind a slow fetch — ping answers while
//! bq runs, cancellation kills the child mid-stream, heartbeats flow.
//! This is the fix for the 1.x defect ("minutes to infinite": the whole
//! server froze behind one tool call and stopped answering ping).

use super::cardio_path;
use serde_json::{json, Value};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Output capture caps — the MCP layer is a CONTEXT window, not a
/// filesystem; oversized output is truncated in-band with a marker.
pub(crate) const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Drain a reader to EOF ALWAYS, keep only the first `cap` bytes.
/// Stopping the read at the cap would let the child block on a full pipe
/// — the deadlock class from the 1.0 sequential capture. Discard the
/// unread tail; it never blocks anyone again.
pub(crate) fn limited_capture(
    mut r: impl Read,
    cap: usize,
    what: &str,
) -> (String, bool) {
    let mut buf = Vec::with_capacity(cap.min(1 << 20));
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.len() >= cap {
            if n > 0 {
                truncated = true;
            }
        } else {
            let take = (cap - buf.len()).min(n);
            buf.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        }
    }
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        s.push_str(&format!(
            "\n...({what} truncated at {cap} bytes by cardio-mcp: narrow with --query/--head/--stats instead of feeding the whole log to the context)\n"
        ));
    }
    (s, truncated)
}

/// MCP progress notification — only with the client's progress token
/// (spec: untokenized progress is a violation; also pure noise).
pub(crate) fn notify_progress(
    tx: &Sender<String>,
    token: &Option<Value>,
    msg: &str,
) {
    if let Some(tok) = token {
        let n = json!({
            "jsonrpc": "2.0", "method": "notifications/progress",
            "params": {"progressToken": tok, "progress": 1, "message": msg}
        });
        let _ = tx.send(n.to_string()); // the writer handles client-gone
    }
}

/// Spawn cardio (the sibling binary) with the tool's argv. CARDIO_BIN
/// overrides for tests and unusual installs; sibling-of-server then PATH.
/// PROCESS GROUP (unix): scripts spawn grandchildren (sleep, gcloud's
/// pipeline) that inherit the output pipes — a bare kill leaves them
/// alive, the capture thread blocks on its pipe until the orphan dies of
/// old age, and cancel/cleanup take 25 s instead of 1 s.
pub(crate) fn spawn_cardio(argv: &[String]) -> Result<Child, String> {
    let mut cmd = Command::new(cardio_path());
    cmd.args(argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn()
        .map_err(|e| {
            format!(
                "cannot run cardio ({}): {e} — is it installed and on PATH?",
                cardio_path()
            )
        })
}

/// Release-free fetch budget: a plain counter; acquire loops with
/// heartbeats, Drop decrements (cancellation paths can't skip it).
/// ponytail: CAS counter — no Mutex in any hot path.
pub(crate) struct FetchBudget {
    pub(crate) active: Arc<AtomicUsize>,
    pub(crate) max: usize,
}

impl FetchBudget {
    pub(crate) fn new(max: usize) -> Self {
        Self { active: Arc::new(AtomicUsize::new(0)), max }
    }
}

impl FetchBudget {
    /// false = cancelled while queued (heartbeats keep flowing meanwhile)
    pub(crate) fn acquire(
        self: &Arc<Self>,
        token: &Option<Value>,
        tx: &Sender<String>,
        cancel: &Arc<AtomicBool>,
    ) -> Option<FetchSlotGuard> {
        loop {
            if cancel.load(Ordering::SeqCst) {
                return None;
            }
            let cur = self.active.load(Ordering::SeqCst);
            if cur >= self.max {
                notify_progress(
                    tx,
                    token,
                    &format!("queued behind {cur} running fetch(es) — waiting for a slot"),
                );
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            match self.active.compare_exchange(
                cur,
                cur + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(FetchSlotGuard(self.clone())),
                Err(_) => continue,
            }
        }
    }
}

pub(crate) struct FetchSlotGuard(Arc<FetchBudget>);
impl Drop for FetchSlotGuard {
    fn drop(&mut self) {
        let _ = self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Execute one cardio job: spawn, capture both pipes concurrently, honor
/// cancellation (kills the process GROUP — orphaned grandchildren held
/// pipes once and will again), heartbeat 10×/s of silence.
pub(crate) fn execute_cardio(
    argv: &[String],
    label: &str,
    token: &Option<Value>,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<String>,
    hard_ceiling_secs: u64,
) -> Result<(String, String, i32, bool), String> {
    let mut child = spawn_cardio(argv)?;
    let Some(mut so) = child.stdout.take() else {
        return Err("cardio stdout missing".into());
    };
    let Some(se) = child.stderr.take() else {
        return Err("cardio stderr missing".into());
    };
    let stderr_handle =
        std::thread::spawn(move || limited_capture(se, MAX_STDERR_BYTES, "stderr"));
    let (tx2, rx2) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = limited_capture(&mut so, MAX_CAPTURE_BYTES, "stdout");
        let _ = tx2.send(out);
    });
    notify_progress(tx, token, &format!("{label} started"));
    let started = Instant::now();
    let mut last_beat = started;
    let outcome = loop {
        if cancel.load(Ordering::SeqCst) {
            break Exec::Cancelled;
        }
        match rx2.recv_timeout(Duration::from_secs(10)) {
            Ok((s, _trunc)) => break Exec::Done(s),
            Err(RecvTimeoutError::Timeout) => {
                let elapsed = started.elapsed().as_secs();
                if last_beat.elapsed().as_secs() >= 10 {
                    last_beat = Instant::now();
                    notify_progress(
                        tx,
                        token,
                        &format!(
                            "still running ({elapsed} s elapsed; big scans can be legitimately slow) — dry-run first on wide scans if unsure"
                        ),
                    );
                }
                // hard wall default only for STREAMING tools is decided by
                // the caller via cancel flag + explicit timeout; here the
                // budget clock is advisory (progress), the hard kill only
                // fires on cancellation
                if hard_ceiling_secs > 0 && elapsed > hard_ceiling_secs.saturating_mul(3) {
                    // sanity ceiling: 3x the streaming default even without
                    // an explicit timeout — nothing runs "to infinity"
                    break Exec::Cancelled;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break Exec::Lost,
        }
    };
    let (stdout, stderr, code, cancelled) = match outcome {
        Exec::Done(s) => {
            let (stderr, _) = stderr_handle
                .join()
                .map_err(|e| format!("stderr capture: {e:?}"))?;
            let code = child
                .wait()
                .map_err(|e| format!("waiting: {e}"))?
                .code()
                .unwrap_or(1);
            (s, stderr, code, false)
        }
        Exec::Cancelled => {
            crate::source::kill_fetcher(&mut child);
            let _ = child.wait();
            let (stderr, _) = stderr_handle.join().unwrap_or((String::new(), false));
            (String::new(), stderr, 1, true)
        }
        Exec::Lost => {
            let _ = child.wait();
            let (stderr, _) = stderr_handle.join().unwrap_or((String::new(), false));
            (String::new(), stderr, 1, false)
        }
    };
    Ok((stdout, stderr, code, cancelled))
}


enum Exec {
    Done(String),
    Cancelled,
    Lost,
}

/// One worker thread's whole body: run the job, send the response.
pub(crate) fn run_and_respond(
    id: Value,
    job: super::tools::ToolJob,
    token: Option<Value>,
    cancel: Arc<AtomicBool>,
    budget: Arc<FetchBudget>,
    tx: Sender<String>,
) {
    let result: Result<(String, String, i32), String> = match job {
        super::tools::ToolJob::Cardio(argv) => {
            let label = tool_label(&argv);
            // hard ceiling: 3x the streaming default keeps an unkillable run
            // from living "to infinity" even without a client cancel
            let ceiling = if budget.max > 0 { 3 * crate::consts::FETCH_TIMEOUT_DEFAULT } else { 0 };
            budget
                .acquire(&token, &tx, &cancel)
                .ok_or_else(|| "cancelled".to_string())
                .and_then(|_slot| execute_cardio(&argv, &label, &token, &cancel, &tx, ceiling))
                .map(|(s, e, c, cancelled)| {
                    let e = if cancelled {
                        format!("{e}\ncancelled — the fetch child was killed on the client's behalf")
                    } else {
                        e
                    };
                    (s, e, c)
                })
        }
        super::tools::ToolJob::Command { program, args, cardio_args, timeout } => {
            let ceiling = timeout.unwrap_or(crate::consts::FETCH_TIMEOUT_DEFAULT);
            execute_command(&program, &args, &cardio_args, &token, &cancel, &tx, ceiling)
                .map(|(s, e, c, cancelled)| {
                    let e = if cancelled {
                        format!("{e}\ncancelled — the fetch child was killed on the client's behalf")
                    } else {
                        e
                    };
                    (s, e, c)
                })
        }
    };
    let line = match result {
        Ok((stdout, stderr, code)) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": tool_result(&stdout, &stderr, code != 0)
        }).to_string(),
        Err(msg) if msg == "cancelled" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"content": [{"type": "text", "text": "cancelled — the fetch child was killed on the client's behalf"}], "isError": true}
        }).to_string(),
        Err(msg) => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"content": [{"type": "text", "text": msg}], "isError": true}
        }).to_string(),
    };
    let _ = tx.send(line);
}

fn tool_label(argv: &[String]) -> String {
    // first NON-flag arg names the tool run (readability in progress msgs)
    argv.iter().find(|a| !a.starts_with('-')).cloned().unwrap_or_else(|| "cardio".into())
}

/// fetch_command: an ARBITRARY provider CLI (az/aws/kubectl/journalctl...)
/// piped into cardio's stdin — the tool that makes "not only for google"
/// literally true. Provider stderr stays INHERITED (its own job notes);
/// cardio's output is captured like every other job.
pub(crate) fn execute_command(
    program: &str,
    args: &[String],
    cardio_args: &[String],
    token: &Option<Value>,
    cancel: &Arc<AtomicBool>,
    tx: &Sender<String>,
    ceiling_secs: u64,
) -> Result<(String, String, i32, bool), String> {
    let mut provider = {
        let mut c = Command::new(program);
        c.args(args).stdout(Stdio::piped()).stderr(Stdio::inherit());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c.spawn().map_err(|e| {
            format!("cannot run {program}: {e} — is the CLI installed and on PATH?")
        })?
    };
    let Some(pout) = provider.stdout.take() else {
        return Err(format!("{program} stdout missing"));
    };
    // THE GENUINE cardio: CARDIO_BIN points at fake tools in tests — using
    // it here would run the provider script as "cardio" again (timestamps
    // would never trim, the run would double-spawn the provider)
    let Some(cardio_bin) = super::sibling_cardio() else {
        return Err("cardio binary not found next to the MCP server; fetch_command always uses the real sibling".into());
    };
    let mut cardio = {
        let mut c = Command::new(cardio_bin);
        c.args(cardio_args)
            .stdin(Stdio::from(pout))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        c.spawn().map_err(|e| format!("cannot run cardio over {program}: {e}"))?
    };
    let Some(mut so) = cardio.stdout.take() else {
        return Err("cardio stdout missing".into());
    };
    let Some(se) = cardio.stderr.take() else {
        return Err("cardio stderr missing".into());
    };
    let stderr_handle = std::thread::spawn(move || limited_capture(se, MAX_STDERR_BYTES, "stderr"));
    let (tx2, rx2) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = limited_capture(&mut so, MAX_CAPTURE_BYTES, "stdout");
        let _ = tx2.send(out);
    });
    notify_progress(tx, token, &format!("{program} | cardio started"));
    let started = Instant::now();
    let mut last_beat = started;
    let outcome = loop {
        if cancel.load(Ordering::SeqCst) {
            break Exec::Cancelled;
        }
        match rx2.recv_timeout(Duration::from_secs(10)) {
            Ok((s, _)) => break Exec::Done(s),
            Err(RecvTimeoutError::Timeout) => {
                if last_beat_check(&mut last_beat) {
                    let elapsed = started.elapsed().as_secs();
                    notify_progress(tx, token, &format!(
                        "still running ({elapsed} s elapsed; big scans can be legitimately slow)"));
                }
                if ceiling_secs > 0 && started.elapsed().as_secs() > ceiling_secs {
                    break Exec::Cancelled;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break Exec::Lost,
        }
    };
    let (stdout, stderr, code, cancelled) = match outcome {
        Exec::Done(s) => {
            let status = provider.wait().map_err(|e| format!("waiting: {e}"))?;
            let (stderr, _) = stderr_handle.join().map_err(|e| format!("stderr capture: {e:?}"))?;
            let ccode = cardio.wait().map_err(|e| format!("waiting: {e}"))?.code().unwrap_or(1);
            // a failing provider IS a failed fetch even when cardio exits 0
            if !status.success() && status.code().unwrap_or(1) != 0 {
                (
                    s,
                    format!("{stderr}\nError: {program} exited with {}", status.code().unwrap_or(1)),
                    status.code().unwrap_or(1),
                    false,
                )
            } else {
                (s, stderr, ccode, false)
            }
        }
        Exec::Cancelled => {
            crate::source::kill_fetcher(&mut provider);
            crate::source::kill_fetcher(&mut cardio);
            let _ = provider.wait();
            let _ = cardio.wait();
            let (stderr, _) = stderr_handle.join().unwrap_or((String::new(), false));
            (String::new(), stderr, 1, true)
        }
        Exec::Lost => {
            let _ = provider.wait();
            let _ = cardio.wait();
            let (stderr, _) = stderr_handle.join().unwrap_or((String::new(), false));
            (String::new(), stderr, 1, false)
        }
    };
    Ok((stdout, stderr, code, cancelled))
}

fn last_beat_check(last: &mut Instant) -> bool {
    if last.elapsed().as_secs() >= 10 {
        *last = Instant::now();
        true
    } else {
        false
    }
}

fn tool_result(stdout: &str, stderr: &str, is_error: bool) -> Value {
    let mut content = Vec::new();
    if !stdout.trim().is_empty() {
        content.push(json!({"type": "text", "text": stdout}));
    }
    if !stderr.trim().is_empty() {
        content.push(json!({"type": "text", "text": format!("stderr notes:\n{stderr}")}));
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": "(empty output, and no notes — the run really produced nothing)"}));
    }
    json!({"content": content, "isError": is_error})
}
