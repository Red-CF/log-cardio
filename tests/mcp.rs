//! The MCP layer, driven the way an MCP client drives it: stdio, one JSON
//! line at a time, against the SAME binary (no fake needed — the server
//! shells out to sibling... to itself, effectively).
//! cfg(unix): the stdio plumbing is identical everywhere; keep one
//! gate for parity with the fetcher-fake tests.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct Server {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
    stdin: std::process::ChildStdin,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn server() -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .arg("--mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cardio --mcp spawns");
    let out = BufReader::new(child.stdout.take().expect("mcp stdout"));
    let stdin = child.stdin.take().expect("mcp stdin");
    Server { child, out, stdin }
}

fn send(srv: &mut Server, msg: &str) {
    writeln!(srv.stdin, "{msg}").expect("mcp write");
    srv.stdin.flush().expect("mcp flush");
}

fn read_response(srv: &mut Server) -> serde_json::Value {
    let mut line = String::new();
    srv.out.read_line(&mut line).expect("mcp response line");
    serde_json::from_str(&line).expect("mcp response parses as JSON")
}

#[test]
fn mcp_handshake_and_tool_list() {
    let mut srv = server();
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let init = read_response(&mut srv);
    assert_eq!(init["result"]["serverInfo"]["name"], "cardio", "got: {init:?}");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    // a notification: no response may come for it
    send(&mut srv, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    send(&mut srv, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let list = read_response(&mut srv);
    let names: Vec<&str> = list["result"]["tools"]
        .as_array().expect("tools array")
        .iter().map(|t| t["name"].as_str().expect("tool name")).collect();
    assert_eq!(
        names.len(), 6,
        "6 tools: {names:?}"
    );
    for want in ["trim_logs", "survey_log", "trace_log", "fetch_gcloud", "fetch_bq", "fetch_k8s"] {
        assert!(names.contains(&want), "tool {want} present: {names:?}");
    }

    // unknown method degrades politely
    send(&mut srv, r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#);
    let err = read_response(&mut srv);
    assert_eq!(err["error"]["code"], -32601, "got: {err:?}");
}

#[test]
fn mcp_trim_logs_tool_call() {
    // unique per test: fixtures are written by this same process' tests
    let path = std::env::temp_dir().join(format!(
        "cardio_mcp_{}_{}.log", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&path, "2026-10-09T12:00:00Z INFO hello from the card\n2026-10-09T12:00:01Z INFO hello again\n")
        .expect("fixture");
    let mut srv = server();
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"trim_logs","arguments":{{"path":"{}"}}}}}}"#,
        path.display()
    );
    send(&mut srv, &call);
    let resp = read_response(&mut srv);
    let result = &resp["result"];
    assert_eq!(result["isError"], false, "trim succeeded: {resp:?}");
    let text: String = result["content"].as_array().expect("content blocks")
        .iter().map(|c| c["text"].as_str().unwrap_or("")).collect();
    assert!(text.contains("hello from the card"), "trimmed lines in content: {text:?}");
    assert!(!text.contains("2026-10-09"), "lean trims the timestamp: {text:?}");
    drop(srv);
    std::fs::remove_file(&path).ok();
}