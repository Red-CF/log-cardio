//! Input sources: file / stdin / child fetchers (gcloud, bq). The fetcher
//! layer owns cost guards — server-side pushdown bounds, project
//! selection, and the bq cost flags agents asked for after real runs.

use crate::timestamps::{until_server_bound, server_ts};
use crate::Args;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio, exit};

/// Input source: byte lines (file/stdin/gcloud) or decoded bq CSV rows.
/// csv::Reader skips the header row and handles quoted fields with
/// embedded newlines/commas — raw --format=csv lines would corrupt them.
pub(crate) enum Source {
    Lines(Box<dyn BufRead>),
    Rows(csv::Reader<ChildStdout>),
}

/// Spawn a child fetcher (gcloud/bq): piped stdout, inherited stderr.
/// Returns (raw stdout pipe, child we own for wait/kill).
pub(crate) fn spawn_fetcher(tool: &'static str, argv: Vec<String>) -> (ChildStdout, Child) {
    let mut child = Command::new(tool)
        .args(&argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("Failed to execute {}: {}. Is the {} CLI installed?", tool, e, tool);
            exit(1);
        });
    let stdout: ChildStdout = child.stdout.take().expect("child stdout");
    (stdout, child)
}

/// Spawn gcloud / bq / open file / stdin. Returns the input source and any
/// child fetcher we own. gcloud mode pushes canonical time bounds into the
/// SERVER-side filter (valid ISO by is_full_iso) to avoid downloading months
/// of entries that local filtering would drop.
/// ponytail: bq mode can't push bounds — the SQL is the user's — so
/// --since/--until filter locally; date it in the query itself.
pub(crate) fn make_reader(
    args: &Args,
    since_valid: &Option<String>,
    until_valid: &Option<String>,
) -> (Source, Option<(&'static str, Child)>) {
    if let Some(filter) = &args.gcloud {
        // The agent's most expensive mistake: an unbounded Cloud Logging
        // read streams the whole retention window. Never launch it silent.
        if since_valid.is_none() {
            eprintln!("Warning: no --since — this gcloud call streams the WHOLE matching range, oldest first. Add --since unless you truly want everything.");
        }
        // PARENTHESIZED: Cloud Logging binds AND tighter than OR — an
        // unparenthesized "a OR b AND timestamp>=…" would bound only the
        // last OR branch and stream the other branch's whole window.
        // bounds already canonicalized: no space separator remains. Pushdown
        // is second-precision (server_ts / until_server_bound): since >= its
        // floor second, until < its next second — the local exact filter
        // trims whatever the wider fetch returns. Comments claim nothing
        // about RFC 3339: these are plain gcloud timestamp literals (no
        // offset), the shape this pushdown has carried since it existed.
        let mut gc_filter = format!("({})", filter);
        if let Some(s) = since_valid {
            gc_filter.push_str(&format!(" AND timestamp>=\"{}\"", server_ts(s)));
        }
        // until: < its next second; None (past 9999-12-31) = no server
        // clause, the local exact filter still bounds the stream
        if let Some(next) = until_valid.as_ref().and_then(|u| until_server_bound(u)) {
            gc_filter.push_str(&format!(" AND timestamp<\"{}\"", next));
        }
        let gc_args = vec![
            "logging".to_string(), "read".to_string(), gc_filter,
            "--format".into(), "value(timestamp,textPayload,jsonPayload.message)".into(),
            "--order=asc".into(),
        ];
        // ponytail: no --limit pushdown; the fetch is bounded by your query
        // (and by the pushdown bounds above)
        let (stdout, child) = spawn_fetcher("gcloud", gc_args);
        (Source::Lines(Box::new(BufReader::new(stdout))), Some(("gcloud", child)))
    } else if let Some(sql) = &args.bq {
        // bq CLI grammar: global flags BEFORE the command, query flags
        // after. --quiet kills the "Waiting on bqjob_…" status chatter
        // agents had to grep away; --project_id replaces the .bigqueryrc/
        // gcloud-config default project (cards and datasets live per
        // environment, executions don't). Cost guards land straight on the
        // query; --bq-args is the escape hatch for anything else.
        let mut bq_args: Vec<String> = Vec::new();
        if let Some(p) = &args.bq_project {
            bq_args.push(format!("--project_id={p}"));
        }
        bq_args.push("--quiet".to_string());
        bq_args.push("query".to_string());
        bq_args.push("--use_legacy_sql=false".to_string());
        bq_args.push("--format=csv".to_string());
        // --max_rows pushed at 10M because bq silently caps selects at
        // 1000 rows; csv::Reader decodes rows (quotes, embedded newlines).
        bq_args.push("--max_rows=10000000".to_string());
        if args.dry_run {
            bq_args.push("--dry_run".to_string());
        }
        if let Some(mb) = args.max_bytes_billed {
            bq_args.push(format!("--maximum_bytes_billed={mb}"));
        }
        if let Some(extra) = &args.bq_args {
            // ponytail: whitespace split, no shlex — document "no quotes
            // inside"; a shelling dep for one rare flag isn't worth it
            bq_args.extend(extra.split_whitespace().map(str::to_string));
        }
        bq_args.push(sql.clone());
        let (stdout, child) = spawn_fetcher("bq", bq_args);
        // ponytail: csv::Reader buffers internally — no BufReader here.
        // flexible(true) tolerates a ragged record instead of aborting.
        let mut rows = csv::ReaderBuilder::new().flexible(true).from_reader(stdout);
        // Column names go to stderr once: the rows themselves join with a
        // tab and the header never pollutes stdout, but an agent correlating
        // trace columns needs the map somewhere. Real bq errors still show
        // (child stderr stays live).
        if let Ok(h) = rows.headers() {
            eprintln!("Note: bq columns: {}", h.iter().collect::<Vec<_>>().join(" | "));
        }
        (Source::Rows(rows), Some(("bq", child)))
    } else if let Some(path) = &args.file_path {
        let file = File::open(path).unwrap_or_else(|e| {
            eprintln!("Error opening file {}: {}", path, e);
            exit(1);
        });
        (Source::Lines(Box::new(BufReader::new(file))), None)
    } else {
        (Source::Lines(Box::new(BufReader::new(io::stdin()))), None)
    }
}