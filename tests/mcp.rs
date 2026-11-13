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
        .stderr(Stdio::inherit())
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
        names.len(), 8,
        "8 tools: {names:?}"
    );
    for want in ["read_log", "survey_log", "trace_log", "census_log",
                 "fetch_gcloud", "fetch_bq", "fetch_k8s", "fetch_command"] {
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
        r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"read_log","arguments":{{"path":"{}"}}}}}}"#,
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
// ---- Server-side fixes driven end to end ----
// CARDIO_BIN -> fake binaries: the deadlock regression (both pipes busy)
// and the progress heartbeat (agent must tell working from hung).

fn fake_cardio(body: &str) -> String {
    let dir = std::env::temp_dir().join(format!("cardio_fakebin_{}", SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).expect("mkdir fakebin");
    let script = dir.join("fake-cardio");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write fake");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod fake");
    }
    dir.join("fake-cardio").to_string_lossy().into_owned()
}

/// Server whose child is a fake CARDIO_BIN (per-call env). Spawn a fresh
/// server with the env baked at spawn time — simpler than mutating after.
fn server_with_bin(bin: &str) -> Server {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .arg("--mcp")
        .env("CARDIO_BIN", bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("cardio --mcp spawns with CARDIO_BIN");
    let out = BufReader::new(child.stdout.take().expect("mcp stdout"));
    let stdin = child.stdin.take().expect("mcp stdin");
    Server { child, out, stdin }
}

#[test]
fn mcp_both_pipes_busy_is_not_a_deadlock() {
    // the 1.0.0 bug: reading stdout to EOF BEFORE stderr let a child that
    // writes to both (bq does) block on a full stderr pipe — the request
    // hung forever. The fake writes 200KB to EACH stream (> both caps and
    // pipe buffers); the server must finish the run in seconds.
    let bin = fake_cardio(
        r#"
        for i in 1 2 3 4 5 6 7 8; do
            head -c 30000 /dev/zero | tr '\0' "s"
            head -c 30000 /dev/zero | tr '\0' "e" >&2
        done
        echo DONE-STDOUT-TAIL
        "#,
    );
    let mut srv = server_with_bin(&bin);
    // initialize first (protocol hygiene)
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let _init = read_response(&mut srv);
    let call = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"fetch_bq","arguments":{"sql":"SELECT 1"}}}"#;
    send(&mut srv, call);
    let resp = read_response(&mut srv);
    assert_eq!(resp["result"]["isError"], false, "no deadlock, run finished: {resp:?}");
    let text: String = resp["result"]["content"].as_array().expect("content blocks")
        .iter().map(|c| c["text"].as_str().unwrap_or("")).collect();
    assert!(text.contains("DONE-STDOUT-TAIL"), "stdout tail kept (keep-first window): {text:?}");
    assert!(text.contains("truncated"), "over-cap stdout is honestly marked: {text:?}");
    drop(srv);
}

#[test]
fn mcp_progress_notifications_flow() {
    // with a progress token, a 12-second child produces at least one
    // heartbeat BEFORE the final response ("working" != "hung")
    let bin = fake_cardio("sleep 12; echo slow-done");
    let mut srv = server_with_bin(&bin);
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let _init = read_response(&mut srv);
    let call = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"_meta":{"progressToken":"tok1"},"name":"fetch_bq","arguments":{"sql":"SELECT 1"}}}"#;
    send(&mut srv, call);
    let mut notifications = 0;
    let mut final_resp = None;
    for _ in 0..20 {
        let msg = read_response(&mut srv);
        if msg.get("id").and_then(|v| v.as_u64()) == Some(9) {
            final_resp = Some(msg);
            break;
        }
        if msg.get("method").and_then(|m| m.as_str()) == Some("notifications/progress") {
            notifications += 1;
        }
    }
    let resp = final_resp.expect("final response arrives");
    assert!(notifications >= 1, "heartbeats seen: {notifications}");
    let text: String = resp["result"]["content"].as_array().expect("content")
        .iter().map(|c| c["text"].as_str().unwrap_or("")).collect();
    assert!(text.contains("slow-done"), "work completed: {text:?}");
    drop(srv);
}

#[test]
fn mcp_no_token_no_noise() {
    // protocol: without a progressToken the server must NOT emit
    // notifications/progress (spec requires the client to ask)
    let bin = fake_cardio("sleep 11; echo done");
    let mut srv = server_with_bin(&bin);
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let _init = read_response(&mut srv);
    send(&mut srv, r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"fetch_bq","arguments":{"sql":"SELECT 1"}}}"#);
    let mut noise = 0;
    loop {
        let msg = read_response(&mut srv);
        if msg.get("id").and_then(|v| v.as_u64()) == Some(2) { break; }
        if msg.get("method").and_then(|m| m.as_str()) == Some("notifications/progress") {
            noise += 1;
        }
    }
    assert_eq!(noise, 0, "untokenized progress is a protocol violation");
    drop(srv);
}

// ---- v2 core: the server must stay ALIVE while a slow tool runs ----
// The 1.x defect the whole redesign exists for: one tool call froze the
// read loop, pings went unanswered, agents saw "minutes to infinite".

fn slow_body() -> String {
    fake_cardio("sleep 12; echo eventually-done")
}

#[test]
fn mcp_ping_answers_while_a_tool_runs() {
    let mut srv = server_with_bin(&slow_body());
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let _init = read_response(&mut srv);
    // start the slow tool
    send(&mut srv, r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"fetch_bq","arguments":{"sql":"SELECT 1"}}}"#);
    // while it runs: a ping MUST answer (the reader loop never blocks)
    send(&mut srv, r#"{"jsonrpc":"2.0","id":50,"method":"ping"}"#);
    let mut pong_seen = false;
    let mut tool = None;
    for _ in 0..10 {
        let msg = read_response(&mut srv);
        if msg.get("id").and_then(|v| v.as_u64()) == Some(50) {
            pong_seen = true;
            assert_eq!(msg["result"], serde_json::json!({}), "ping result: {msg:?}");
        }
        if msg.get("id").and_then(|v| v.as_u64()) == Some(9) {
            tool = Some(msg);
            break;
        }
    }
    assert!(pong_seen, "pong arrived while the tool ran");
    let resp = tool.expect("tool response eventually");
    let text: String = resp["result"]["content"].as_array().expect("content")
        .iter().map(|c| c["text"].as_str().unwrap_or("")).collect();
    assert!(text.contains("eventually-done"), "tool finished: {text:?}");
    drop(srv);
}

#[test]
fn mcp_cancellation_kills_the_child() {
    // the slow child is told to mark a file when it WAKES UP; if the
    // server only "pretended" to cancel, the file would appear later
    let marker = std::env::temp_dir().join(format!(
        "cardio_cancel_{}.marker", SEQ.fetch_add(1, Ordering::Relaxed)));
    let body = format!(
        "sleep 25; touch {}",
        marker.to_string_lossy().replace('\\', "/")
    );
    let bin = fake_cardio(&body);
    let mut srv = server_with_bin(&bin);
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let _init = read_response(&mut srv);
    send(&mut srv, r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"fetch_bq","arguments":{"sql":"SELECT 1"}}}"#);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let cancel_t0 = std::time::Instant::now();
    send(&mut srv, r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#);
    let resp = read_response(&mut srv);
    let elapsed = cancel_t0.elapsed();
    assert_eq!(resp.get("id").and_then(|v| v.as_u64()), Some(9), "cancelled call answers its id: {resp:?}");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "cancellation is immediate, not after 25 s: {elapsed:?}"
    );
    let text: String = resp["result"]["content"].as_array().expect("content")
        .iter().map(|c| c["text"].as_str().unwrap_or("")).collect();
    assert!(text.contains("cancelled"), "honest result: {text:?}");
    drop(srv);
    let _ = std::fs::remove_file(&marker);
}

#[cfg(unix)]
#[test]
fn mcp_fetch_command_is_provider_neutral() {
    // ANY CLI: here, a stub "log provider" piped through the lean pipeline
    // — the tool that makes "not only for google" literally true
    let provider = fake_cardio("echo 2026-11-12T10:00:00 INFO hello-azure; echo 2026-11-12T10:00:01 INFO more");
    let mut srv = server_with_bin(&provider);
    send(&mut srv, r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    let _init = read_response(&mut srv);
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{{"name":"fetch_command","arguments":{{"command":"{}","args":["--flag"]}} }}}}"#,
        provider
    );
    send(&mut srv, &call);
    let resp = read_response(&mut srv);
    assert_eq!(resp["result"]["isError"], false, "provider pipe worked: {resp:?}");
    let text: String = resp["result"]["content"].as_array().expect("content")
        .iter().map(|c| c["text"].as_str().unwrap_or("")).collect();
    assert!(text.contains("hello-azure"), "provider stdout flowed: {text:?}");
    assert!(!text.contains("2026-11-12"), "lean trimmed the ts: {text:?}");
    drop(srv);
}
