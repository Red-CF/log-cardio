//! MCP stdio server, v2 concurrent core. One thread owns the protocol
//! read loop (it NEVER blocks on a tool — workers do that), one thread
//! owns stdout, one thread per tools/call. This is the fix for the 1.x
//! defect: the whole server used to freeze behind one slow fetch and
//! stop answering ping ("minutes to infinite, agents give up").
//!
//! ponytail: still hand-rolled (serde_json only) — the concurrency is
//! reader/writer/worker over channels, no async runtime. The migration
//! ceiling stays documented: rmcp earns its keep when HTTP transport,
//! dynamic resources or prompts land.

mod tools;
mod worker;
mod writer;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufRead;
use std::process::exit;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use worker::FetchBudget;

pub(crate) fn serve() {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let writer = writer::spawn_writer(std::io::stdout(), rx);

    let budget = Arc::new(FetchBudget::new(crate::consts::MAX_CONCURRENT_FETCHES));
    // request id -> its cancellation flag; touched by the reader (insert)
    // and by cancel notifications (flip) — the only shared mutable state
    let registry: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // The read loop IS the reader thread: dispatch never blocks here —
    // every tools/call defers to a worker thread before returning.
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            let _ = tx.send(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": "parse error"}
            }).to_string());
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // no method: responses are ignored per JSON-RPC; broken
            // requests answer -32600 (with an id to address)
            if id.is_null()
                && msg.get("result").is_none()
                && msg.get("error").is_none()
            {
                continue;
            }
            if !id.is_null() && msg.get("result").is_none() && msg.get("error").is_none() {
                let _ = tx.send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32600, "message": "invalid request (no method)"}
                }).to_string());
            }
            continue;
        };
        if id.is_null() {
            // notifications of ANY prefix are never responded to
            if method == "notifications/cancelled" {
                // ...except they DO act: flip their request's cancel flag
                let target = msg
                    .get("params")
                    .and_then(|p| p.get("requestId"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let key = target.to_string();
                if let Ok(mut reg) = registry.lock() {
                    if let Some(flag) = reg.remove(&key) {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
            }
            continue;
        }

        match method {
            "initialize" => {
                // echo the client's requested version — negotiation-free
                let ver = msg
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .cloned()
                    .unwrap_or(json!("2025-06-18"));
                let _ = tx.send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": ver,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "cardio", "version": env!("CARGO_PKG_VERSION")}
                    }
                }).to_string());
            }
            "ping" => {
                // answered from the read loop: while a 20-minute bq runs,
                // the client still sees a live server — THE 1.x defect
                let _ = tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string());
            }
            "tools/list" => {
                let _ = tx.send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"tools": &*tools::SCHEMA}
                }).to_string());
            }
            "tools/call" => {
                let Some(params) = msg.get("params").cloned() else {
                    let _ = tx.send(json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": "missing params"}
                    }).to_string());
                    continue;
                };
                let token = params
                    .get("_meta")
                    .and_then(|m| m.get("progressToken"))
                    .cloned();
                let job = match tools::execute(
                    params.get("name").and_then(Value::as_str).unwrap_or(""),
                    params.get("arguments").cloned().unwrap_or(json!({})),
                ) {
                    Ok(j) => j,
                    Err(e) => {
                        let _ = tx.send(json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": e}], "isError": true}
                        }).to_string());
                        continue;
                    }
                };
                // registry + cancel flag + worker spawn — the read loop is
                // free again within microseconds of the spawn
                let cancel = Arc::new(AtomicBool::new(false));
                if let Ok(mut reg) = registry.lock() {
                    reg.insert(id.to_string(), cancel.clone());
                }
                let tx2 = tx.clone();
                let reg2 = registry.clone();
                // budget cloned ONCE per call; the closure owns the Arc
                let budget2 = budget.clone();
                handles.push(std::thread::spawn(move || {
                    // key for the registry cleanup AFTER the id move
                    let key = id.to_string();
                    worker::run_and_respond(id, job, token, cancel, budget2, tx2);
                    if let Ok(mut reg) = reg2.lock() {
                        reg.remove(&key);
                    }
                }));
            }
            other => {
                // unknown REQUEST: spec degrade (only for id-bearing calls)
                let _ = tx.send(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("unknown method: {other}")}
                }).to_string());
            }
        }
    }

    // stdin EOF: let running workers finish (their results still get sent),
    // then close the writer. Cancel-at-EOF is not a client semantics we
    // can honor — the protocol channel is gone anyway.
    for h in handles {
        let _ = h.join();
    }
    drop(tx);
    let _ = writer.join();
    exit(0);
}

/// The sibling cardio binary: CARDIO_BIN override (tests, unusual
/// installs), the directory this server binary lives in, then PATH.
/// The REAL cardio binary (sibling of this server) — fetch_command always
/// pipes into the GENUINE cardio, never CARDIO_BIN (which points at fake
/// tools in tests; pointing "cardio" at the provider itself would loop).
pub(crate) fn sibling_cardio() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let p = exe
        .parent()?
        .join(if cfg!(windows) { "cardio.exe" } else { "cardio" });
    p.exists().then(|| p.to_string_lossy().into_owned())
}

pub(crate) fn cardio_path() -> String {
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
