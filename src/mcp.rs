//! cardio --mcp: an MCP stdio server that wraps THIS SAME binary as agent
//! tools. Engine reuse is a shell-out to the cardio binary: the CLI is
//! the single contract (flags, stderr notes, cost guards) and this layer
//! stays thin. Same-binary = zero version skew (the tools reference
//! exactly the flags this build ships).
//!
//! ponytail: hand-rolled JSON-RPC/MCP over stdio lines (serde_json only).
//! Ceiling: implements the handshake, notifications-ignore, tools/list,
//! tools/call and -32601 for everything else — no prompts, no resources,
//! no sampling. Migrate to `rmcp` when a richer surface is wanted.

mod tools;

use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::process::Command;

/// Find the cardio binary: the server's own directory (the layout ships
/// both targets together), then PATH.
fn cardio_path() -> String {
    // CARDIO_BIN: the escape hatch for tests (fake binaries) and unusual
    // installs; sibling-of-server then PATH come next, that order
    if let Ok(p) = std::env::var("CARDIO_BIN") {
        return p;
    }
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
        let p = dir.join(if cfg!(windows) { "cardio.exe" } else { "cardio" });
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    "cardio".to_string()
}

/// Output capture caps: an MCP trim_logs --raw on a 10GB file must not
/// OOM the (shared) agent runtime by building one giant String; the tool
/// result is a CONTEXT window, not a filesystem. stdout carries a
/// truncation marker; the stderr notes (stdout truncated too) say it.
const MAX_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Read a reader to EOF, keeping at most cap bytes (+ the marker dropped
/// beyond it). Unread remainder: the child gets EPIPE and exits — its
/// status still lands in the caller's wait().
fn limited_capture(
    mut r: impl std::io::Read,
    cap: usize,
    what: &str,
) -> (String, bool) {
    // Drain to EOF ALWAYS, keep only the first `cap` bytes: stopping the
    // read at the cap lets the child block on a full pipe — with both
    // streams captured sequentially that is a deadlock (a real agent run
    // hung 30 minutes on exactly this), so the unread tail is discarded,
    // never left unread.
    let mut buf = Vec::with_capacity(cap.min(1 << 20));
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false; // anything read past the cap
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.len() >= cap {
            if n > 0 { truncated = true; }
        } else {
            let take = (cap - buf.len()).min(n);
            buf.extend_from_slice(&chunk[..take]);
            if take < n { truncated = true; }
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

/// MCP progress notification — only sent when the caller provided a
/// progress token; without one, silently skipped (no noise, no protocol
/// violation: the spec forbids untokenized progress).
fn notify_progress(out: &mut impl Write, token: &Option<Value>, msg: &str) {
    if let Some(tok) = token {
        let n = json!({
            "jsonrpc": "2.0", "method": "notifications/progress",
            "params": {"progressToken": tok, "progress": 1, "message": msg}
        });
        // client gone mid-run: EPIPE tolerable, keep collecting for the
        // in-flight request; the channel closes after this response anyway
        writeln!(out, "{n}").ok();
        out.flush().ok();
    }
}

/// Run cardio with the args the tool call built. stdout = the trimmed
/// stream (CAPPED, see MAX_CAPTURE_BYTES); stderr = cardio's honest
/// notes — both go back to the agent. Spawn failures surface as a tool
/// error, never kill the server. `token` (client's progressToken) turns
/// on progress notifications: bq scans run minutes — without a heartbeat
/// the agent sees silence and "working" is indistinguishable from "hung"
/// (a real agent run aborted a healthy query after 30 muted minutes).
fn run_cardio(
    args: &[String],
    token: &Option<Value>,
    out: &mut impl Write,
) -> Result<(String, String, i32), String> {
    let mut child = Command::new(cardio_path())
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run cardio ({}): {e} — is it installed and on PATH?", cardio_path()))?;
    let Some(mut so) = child.stdout.take() else { return Err("cardio stdout missing".into()) };
    let Some(se) = child.stderr.take() else { return Err("cardio stderr missing".into()) };
    // BOTH streams must be drained concurrently: bq streams its stdout
    // while writing job notes to stderr — reading one to EOF before the
    // other deadlocks the child on a full pipe (real 30-minute hang).
    let stderr_handle = std::thread::spawn(move || limited_capture(se, MAX_STDERR_BYTES, "stderr"));
    // stdout ALSO on a thread: main must stay free to write progress
    // heartbeats, and std pipes have no timeout read — the wait happens
    // over a channel instead of the descriptor.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = limited_capture(&mut so, MAX_CAPTURE_BYTES, "stdout");
        let _ = tx.send(out); // receiver never dropped before this fires
        drop(tx);
    });
    notify_progress(out, token, "fetch started — large scans can take minutes");
    let started = std::time::Instant::now();
    let (stdout, stderr) = {
        // heartbeat loop: every 10s of silence = one progress note, so
        // "working" is never indistinguishable from "hung" again
        let mut captured = None;
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok((s, _trunc)) => {
                    captured = Some(s);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    notify_progress(out, token, &format!(
                        "still running ({} s elapsed; big scans can be legitimately slow) — dry-run first on wide scans if unsure",
                        started.elapsed().as_secs()));
                }
                Err(_) => {
                    eprintln!("cardio-mcp: stdout capture lost — result may be empty");
                    break;
                }
            }
        }
        let stdout = captured.unwrap_or_default();
        let (stderr, _err_trunc) = stderr_handle
            .join()
            .map_err(|e| format!("stderr capture: {e:?}"))?;
        (stdout, stderr)
    };
    let status = child.wait().map_err(|e| format!("waiting on cardio: {e}"))?;
    Ok((stdout, stderr, status.code().unwrap_or(1)))
}

/// MCP tools/call result: each block is free-form text; empty stdout with
/// notes is still a valid, useful answer (an empty result WITH its witness).
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

fn dispatch<W: std::io::Write>(
    method: &str,
    params: Option<Value>,
    id: Value,
    out: &mut W,
) -> Option<Value> {
    match method {
        "initialize" => {
            // echo the client's requested version: servers MAY answer with
            // the requested one — the negotiation-free path
            let ver = params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .cloned()
                .unwrap_or(json!("2025-06-18"));
            Some(json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": ver,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "cardio", "version": env!("CARGO_PKG_VERSION")}
                }
            }))
        }
        "tools/list" => Some(json!({
            "jsonrpc": "2.0", "id": id,
            "result": {"tools": &*tools::SCHEMA}
        })),
        "tools/call" => {
            let Some(params) = params else {
                return Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "missing params"}}));
            };
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let token = params
                .get("_meta")
                .and_then(|m| m.get("progressToken"))
                .cloned();
            match tools::execute(name, params.get("arguments").cloned().unwrap_or(json!({})))
                .and_then(|argv| run_cardio(&argv, &token, out))
            {
                Ok((stdout, stderr, code)) => {
                    Some(json!({"jsonrpc": "2.0", "id": id, "result": tool_result(&stdout, &stderr, code != 0)}))
                }
                Err(e) => {
                    Some(json!({"jsonrpc": "2.0", "id": id, "result": tool_result("", &e.to_string(), true)}))
                }
            }
        }
        // notifications carry no id: nothing to answer, nothing to store
        other if other.starts_with("notifications/") => None,
        // every other method: spec-compliant degrade
        _ => Some(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("unknown method: {method}")}
        })),
    }
}

/// The one runtime loop: line-delimited JSON-RPC over stdio until EOF.
pub(crate) fn serve() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let resp = json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": "parse error"}
            });
            writeln!(out, "{resp}").ok();
            out.flush().ok();
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // no method: a RESPONSE (has result/error) must be IGNORED
            // per JSON-RPC; anything else with an id is a broken request
            if id.is_null()
                && msg.get("result").is_none() && msg.get("error").is_none() {
                continue;
            }
            if !id.is_null()
                && msg.get("result").is_none() && msg.get("error").is_none() {
                let bad = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": "invalid request (no method)"}});
                if writeln!(out, "{bad}").is_err() { break; }
                out.flush().ok();
            }
            continue;
        };
        if id.is_null() {
            // a NOTIFICATION, whatever its prefix ($/cancelRequest…):
            // no response is ever sent for those
            continue;
        }
        match dispatch(method, msg.get("params").cloned(), id, &mut out) {
            Some(resp) => {
                // client gone (killed, disconnected): quiet exit, not a panic
                if writeln!(out, "{resp}").is_err() {
                    break;
                }
                if out.flush().is_err() {
                    break;
                }
            }
            None => continue,
        }
    }
}