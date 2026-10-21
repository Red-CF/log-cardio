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
/// On unix the child gets its OWN process group: kill() would otherwise
/// leave grandchildren (a `sleep` inside a wrapper script) holding the
/// pipe write-end open, and main would block on the pipe until the
/// orphan died of old age (a 30 s "hung" run was exactly this).
/// Returns (raw stdout pipe, child we own for wait/kill).
pub(crate) fn spawn_fetcher(tool: &'static str, argv: Vec<String>) -> (ChildStdout, Child) {
    let mut cmd = Command::new(tool);
    cmd.args(&argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("Failed to execute {}: {}. Is the {} CLI installed?", tool, e, tool);
        exit(1);
    });
    let stdout: ChildStdout = child.stdout.take().expect("child stdout");
    (stdout, child)
}

/// Kill a fetcher AND its whole process group when possible (unix): a
/// plain Child::kill leaves orphaned grandchildren holding the output
/// pipe. The kill(1) subprocess is dependency-free; the plain kill falls
/// back for the no-group case (windows / no kill binary).
/// ponytail: libc::killpg if the spawn or edge cases ever bother anyone.
pub(crate) fn kill_fetcher(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    let _ = Command::new("kill").arg("-9").arg(format!("-{pid}")).status();
    let _ = child.kill();
}

/// --timeout watchdog: after `secs` of the child still being alive, kill
/// the process group and say so. secs == 0 disables (explicit --timeout 0).
/// Default policy (main's): STREAMING fetchers only (gcloud/k8s) — a bq
/// job sits silent until done, so a wall-clock there kills legit long
/// scans; bq needs an explicit --timeout.
pub(crate) fn spawn_watchdog(child: &std::sync::Arc<std::sync::Mutex<Child>>, secs: u64) {
    if secs == 0 {
        return;
    }
    let watchdog = std::sync::Arc::clone(child);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        let Ok(mut c) = watchdog.lock() else { return };
        match c.try_wait() {
            // reaped or gone: nothing to kill (kill on a reaped pid could
            // hit an unrelated process)
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => {
                kill_fetcher(&mut c);
                eprintln!("Error: fetcher killed after --timeout {}s (its job id is visible in the output above).", secs);
            }
        }
    });
}

/// Reap the fetcher after the run: kill on our own early cut, then poll
/// try_wait without ever holding the mutex across a sleep (the watchdog
/// needs the lock; a guard held across the poll-sleep deadlocks it).
/// Failures of the child itself propagate as Fail::Fetch.
pub(crate) fn collect_fetcher(
    child: &std::sync::Arc<std::sync::Mutex<Child>>,
    tool: Option<&'static str>,
    early_cut: bool,
) -> Result<(), crate::errors::Fail> {
    if early_cut {
        let mut g = child.lock().unwrap();
        kill_fetcher(&mut g);
    }
    let status;
    loop {
        let probed = { child.lock().unwrap().try_wait() };
        match probed {
            Ok(Some(s)) => { status = Some(s); break; }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                eprintln!("Warning: failed to wait for {}: {}", tool.unwrap_or("fetcher"), e);
                status = None;
                break;
            }
        }
    }
    let Some(status) = status else { return Ok(()) };
    if !status.success() && !early_cut {
        // Propagate: scripts piping cardio must see fetch failures.
        // (An early-cut kill is OUR failure, already reported — ignore it.)
        return Err(crate::errors::Fail::Fetch(format!(
            "{} exited with {}", tool.unwrap_or("fetcher"), status)));
    }
    Ok(())
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
    } else if let Some(selector) = &args.k8s {
        // kubectl logs selector (pod/name, deployment/name, bare name);
        // the namespace and context are kubectl's own (current ones).
        // The agent's most expensive mistake is still an unbounded stream.
        if since_valid.is_none() {
            eprintln!("Warning: no --since — this kubectl logs call streams from pod creation. Add --since unless you want everything.");
        }
        let mut k8s_args: Vec<String> = vec!["logs".into(), selector.clone()];
        // --timestamps is REQUIRED for the local --until to exist: without
        // it kubectl emits ts-less lines and the range filter bypasses
        // timestamp-less lines entirely (silent no-op, review-caught)
        k8s_args.push("--timestamps".into());
        if let Some(c) = &args.k8s_container {
            k8s_args.push("-c".into());
            k8s_args.push(c.clone());
        }
        // since pushdown: kubectl wants RFC3339 — canonical without frac,
        // UTC 'Z' appended (the tz-naive caveat is the same as gcloud's).
        // ponytail: pushdown is the ONLY reason --k8s exists — the trim
        // engine works through a plain kubectl pipe without it.
        if let Some(s) = since_valid {
            k8s_args.push(format!("--since-time={}Z", server_ts(s)));
        }
        let (stdout, child) = spawn_fetcher("kubectl", k8s_args);
        (Source::Lines(Box::new(BufReader::new(stdout))), Some(("kubectl", child)))
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