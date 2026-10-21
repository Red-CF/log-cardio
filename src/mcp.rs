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
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
        let p = dir.join(if cfg!(windows) { "cardio.exe" } else { "cardio" });
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    "cardio".to_string()
}

/// Run cardio with the args the tool call built. stdout = the trimmed
/// stream; stderr = cardio's honest notes — both go back to the agent.
fn run_cardio(args: &[String]) -> (String, String, i32) {
    let out = Command::new(cardio_path())
        .args(args)
        .output()
        .unwrap_or_else(|e| {
            eprintln!("cardio-mcp: cannot run cardio: {e}");
            std::process::exit(1);
        });
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(1),
    )
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

fn dispatch(method: &str, params: Option<Value>, id: Value) -> Option<Value> {
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
            match tools::execute(name, params.get("arguments").cloned().unwrap_or(json!({}))) {
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
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        match dispatch(method, msg.get("params").cloned(), id) {
            Some(resp) => {
                writeln!(out, "{resp}").expect("stdout write");
                out.flush().expect("stdout flush");
            }
            None => continue,
        }
    }
}