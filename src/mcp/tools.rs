//! Static tool schema + dispatcher for the MCP layer (see src/mcp.rs).
//! Each tool mirrors one cardio CLI mode; arguments map 1:1 to flags, so
//! the CLI contract (stderr notes, caps, guards) is what the caller gets.
//! Schemas are minimal JSON Schema: only what the calling agent must
//! decide.

use serde_json::{json, Value};
use std::sync::LazyLock;

pub(crate) static SCHEMA: LazyLock<Vec<Value>> = LazyLock::new(|| vec![
    json!({
        "name": "trim_logs",
        "description": "Read a log file through cardio's lean pipeline: timestamps/boilerplate trimmed, near-duplicate lines dropped (exact for the first 5000 unique lines, then bloom with ~1% false positives), and a window of kept lines (default: LAST 2000 for streams/files, FIRST 2000 for cloud fetches). Fields >1000 chars cut with a marker in lean mode. Returns the kept lines plus cardio's honest stderr notes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Log file path (file reads only: the server's stdio belongs to MCP itself)"},
                "query": {"type": "string", "description": "Keep only lines containing this substring"},
                "tail": {"type": "number", "description": "Keep only the LAST N lines (most recent)"},
                "head": {"type": "number", "description": "Keep only the FIRST N lines"},
                "skip": {"type": "number", "description": "Drop the first N kept lines before emitting"},
                "raw": {"type": "boolean", "description": "Verbatim: no trim, no dedup, no cap, no field cut"},
                "json": {"type": "boolean", "description": "NDJSON output with input line indices"},
                "max_field_len": {"type": "number", "description": "Cut fields longer than N chars (0 = unlimited; lean default 1000)"}
            },
            "required": ["path"]
        }
    }),
    json!({
        "name": "survey_log",
        "description": "Pattern survey of a log file: top-20 frequencies with digit runs folded to '#', so per-id noise groups into one pattern. No stream output. First move on any unknown log.",
        "inputSchema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        }
    }),
    json!({
        "name": "trace_log",
        "description": "Group a log's lines by a captured ID regex (e.g. 'request_id=([a-z0-9-]+)'), full sessions or compact summary. No dedup in trace mode: every event survives.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "trace": {"type": "string", "description": "Regex with a capture group for the ID"},
                "summary": {"type": "boolean", "description": "Compact per-ID map (count + first/last) instead of full sessions"},
                "top": {"type": "number", "description": "Keep only the biggest N sessions"},
                "head": {"type": "number", "description": "Cap collected events at N (sessions keep earliest)"}
            },
            "required": ["path", "trace"]
        }
    }),
    json!({
        "name": "fetch_gcloud",
        "description": "Stream Cloud Logging entries matching a filter through cardio. --since/--until are pushed into the server-side timestamp filter, so only the asked window downloads.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "filter": {"type": "string", "description": "Cloud Logging filter, e.g. resource.type=gce_instance"},
                "since": {"type": "string", "description": "ISO timestamp (or YYYY-MM-DD); strongly recommended — without it the whole matching range streams"},
                "until": {"type": "string", "description": "Exclusive upper ISO bound"}
            },
            "required": ["filter"]
        }
    }),
    json!({
        "name": "fetch_bq",
        "description": "Run a BigQuery SQL through the bq CLI and feed rows through cardio's pipeline. Prefer explicit column SELECTs over TO_JSON_STRING(payload); FORMAT_TIMESTAMP the timestamp column to ISO. Cost guards are in the tool: dry-run estimate first on wide scans, hard billed-bytes cap.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "sql": {"type": "string"},
                "dry_run": {"type": "boolean", "description": "Cost estimate only (no rows read) — run this FIRST on wide/unknown scans"},
                "max_bytes_billed": {"type": "number", "description": "Hard billed-bytes cap for the job"},
                "project": {"type": "string", "description": "GCP project that runs the job"},
                "bq_args": {"type": "string", "description": "Raw bq CLI flags passthrough, split on whitespace"}
            },
            "required": ["sql"]
        }
    }),
    json!({
        "name": "fetch_k8s",
        "description": "Stream one pod/deployment's logs (kubectl logs) through cardio. Time bounds: --since pushes to kubectl --since-time (UTC); --until filters locally. The pod/deployment selector uses the current kubectl context and namespace.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "selector": {"type": "string", "description": "kubectl logs target, e.g. 'pod/my-api-abc123', 'deployment/my-api', 'my-api'"},
                "container": {"type": "string", "description": "-c container name (optional)"},
                "since": {"type": "string", "description": "ISO or YYYY-MM-DD, pushed server-side"},
                "until": {"type": "string", "description": "Exclusive upper ISO bound, filtered locally (kubectl has no until pushdown)"},
                "query": {"type": "string", "description": "Keep only lines containing this substring"}
            },
            "required": ["selector"]
        }
    }),
]);

/// Dispatch a tools/call. Errors here become isError=true results (the
/// agent sees the reason); cardio's own failures propagate the same way.
pub(crate) fn execute(name: &str, args: Value) -> Result<Vec<String>, String> {
    let s = |key: &str| args.get(key).and_then(Value::as_str);
    let b = |key: &str| args.get(key).and_then(Value::as_bool);
    let mut argv: Vec<String> = Vec::new();

    // The schemas say "number"; JSON numbers arrive as f64. Accept
    // integral ones (1000.0 == 1000), reject the rest LOUDLY — a silently
    // dropped cap would run uncapped instead of erroring.
    fn num(args: &Value, key: &str) -> Result<Option<u64>, String> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(x)) => {
                if let Some(v) = x.as_u64() {
                    Ok(Some(v))
                } else if let Some(f) = x.as_f64() {
                    if f.fract() == 0.0 && f >= 0.0 {
                        Ok(Some(f as u64))
                    } else {
                        Err(format!("{key}: expected an integer number, got {x}"))
                    }
                } else {
                    Err(format!("{key}: expected an integer number, got {x}"))
                }
            }
            Some(other) => Err(format!("{key}: expected a number, got {other}")),
        }
    }
    let head = num(&args, "head")?;
    let tail = num(&args, "tail")?;
    let skip = num(&args, "skip")?;
    let field_len = num(&args, "max_field_len")?;
    let top = num(&args, "top")?;
    let bytes = num(&args, "max_bytes_billed")?;

    match name {
        "trim_logs" => {
            let Some(path) = s("path") else { return Err("missing: path".into()) };
            if path == "-" || path.is_empty() {
                return Err("trim_logs reads FILES only (the server's own stdio is the MCP channel); pipe through the cardio CLI for stdin sources".into());
            }
            argv.push(path.into());
            if let Some(q) = s("query") { argv.extend(["--query".into(), q.into()]); }
            if let Some(v) = tail { argv.extend(["--tail".into(), v.to_string()]); }
            if let Some(v) = head { argv.extend(["--head".into(), v.to_string()]); }
            if let Some(v) = skip { argv.extend(["--skip".into(), v.to_string()]); }
            if b("raw").unwrap_or(false) { argv.push("--raw".into()); }
            if b("json").unwrap_or(false) { argv.push("--json".into()); }
            if let Some(v) = field_len { argv.extend(["--max-field-len".into(), v.to_string()]); }
        }
        "survey_log" => {
            let Some(path) = s("path") else { return Err("missing: path".into()) };
            argv.extend([path.into(), "--stats".into(), "--json".into()]);
        }
        "trace_log" => {
            let (Some(path), Some(trace)) = (s("path"), s("trace")) else {
                return Err("missing: path and trace".into());
            };
            argv.push(path.into());
            argv.extend(["--trace".into(), trace.into()]);
            if b("summary").unwrap_or(false) { argv.push("--summary".into()); }
            if let Some(v) = top { argv.extend(["--top".into(), v.to_string()]); }
            if let Some(v) = head { argv.extend(["--head".into(), v.to_string()]); }
        }
        "fetch_gcloud" => {
            let Some(filter) = s("filter") else { return Err("missing: filter".into()) };
            argv.extend(["--gcloud".into(), filter.into()]);
            if let Some(v) = s("since") { argv.extend(["--since".into(), v.into()]); }
            if let Some(v) = s("until") { argv.extend(["--until".into(), v.into()]); }
        }
        "fetch_bq" => {
            let Some(sql) = s("sql") else { return Err("missing: sql".into()) };
            argv.extend(["--bq".into(), sql.into()]);
            if b("dry_run").unwrap_or(false) { argv.push("--dry-run".into()); }
            if let Some(v) = bytes { argv.extend(["--max-bytes-billed".into(), v.to_string()]); }
            if let Some(v) = s("project") { argv.extend(["--project".into(), v.into()]); }
            if let Some(v) = s("bq_args") { argv.extend(["--bq-args".into(), v.into()]); }
        }
        "fetch_k8s" => {
            let Some(sel) = s("selector") else { return Err("missing: selector".into()) };
            argv.extend(["--k8s".into(), sel.into()]);
            if let Some(c) = s("container") { argv.extend(["-c".into(), c.into()]); }
            if let Some(v) = s("since") { argv.extend(["--since".into(), v.into()]); }
            if let Some(v) = s("until") { argv.extend(["--until".into(), v.into()]); }
            if let Some(q) = s("query") { argv.extend(["--query".into(), q.into()]); }
        }
        other => return Err(format!("unknown tool: {other}")),
    }
    Ok(argv)
}
