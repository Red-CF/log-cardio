//! cardio — trim noise from logs for LLM agents. Mode wiring lives here;
//! pipeline/sessions/source/timestamps each own their layer (see the
//! module docs). Keep main.rs the smallest file: new behavior belongs in
//! the layer it affects.

mod agent_md;
mod mcp;
mod pipeline;
mod sessions;
mod source;
mod timestamps;

use bloomfilter::Bloom;
use clap::Parser;
use regex::Regex;
use sessions::{Run, RunResult, emit_sessions};
use pipeline::{Pipeline, Sink, flush, run_stream};
use source::make_reader;
use std::collections::HashMap;
use std::io;
use std::process::exit;
use std::sync::LazyLock;
use timestamps::{TRIM_BODY, canonical_bound, is_full_iso};

/// --stats survey cap: unique patterns tracked before the counter stops
const MAX_STATS_PATTERNS: usize = 100_000;
/// --lean output cap: enough events for a debugging session, small
/// enough that an agent never swallows a token wall by accident.
const LEAN_MAX_LINES: usize = 2_000;

#[derive(Parser, Debug)]
#[command(author, version, about = "Trim noise from logs for LLM agents")]
pub(crate) struct Args {
    /// Path to the log file. If omitted, reads from stdin or GCloud.
    pub(crate) file_path: Option<String>,

    /// Filter lines containing this text (applied before trimming)
    #[arg(long)]
    pub(crate) query: Option<String>,

    /// Remove duplicate lines
    #[arg(long)]
    pub(crate) dedup: bool,

    /// Keep only the LAST N lines — the most recent beat the old (whole
    /// input reads; event count in trace mode; ignored with --stats)
    #[arg(long)]
    pub(crate) max_lines: Option<usize>,

    /// Output in JSON format
    #[arg(long)]
    pub(crate) json: bool,

    /// Only the last N kept lines (after dedup/query). Reads the WHOLE
    /// input — the line cap does not apply; the tail replaces it. The
    /// #1 agent read: "what did the log just say".
    #[arg(long)]
    pub(crate) tail: Option<usize>,

    /// Only lines with timestamp >= this (ISO, or YYYY-MM-DD = day start).
    #[arg(long)]
    pub(crate) since: Option<String>,

    /// Only lines with timestamp < this (ISO, or YYYY-MM-DD = next-day start).
    #[arg(long)]
    pub(crate) until: Option<String>,

    /// Group logs by a captured ID (e.g., "request_id=([a-z0-9-]+)")
    #[arg(long)]
    pub(crate) trace: Option<String>,

    /// Compact per-ID map (event count + first/last event, IDs preserved)
    /// instead of full sessions; needs --trace or config default_trace
    #[arg(long)]
    pub(crate) summary: bool,

    /// Survey mode: frequency summary of the whole file to stderr, no stream output
    #[arg(long)]
    pub(crate) stats: bool,

    /// GCloud Logging filter (e.g. "resource.type=gce_instance")
    #[arg(long)]
    pub(crate) gcloud: Option<String>,

    /// BigQuery SQL — runs the bq CLI (CSV output) and feeds rows into the
    /// same pipeline (query/range/trace/summary/stats)
    #[arg(long)]
    pub(crate) bq: Option<String>,

    /// GCP project that runs the bq job — by default the CLI's configured
    /// project (cards/datasets live per environment, executions don't)
    #[arg(long = "project")]
    pub(crate) bq_project: Option<String>,

    /// Upper limit on bytes this bq query may bill (errors instead of
    /// scanning multi-TB tables by accident)
    #[arg(long)]
    pub(crate) max_bytes_billed: Option<u64>,

    /// Estimate cost only: run `bq query --dry_run` and stop (no rows read)
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Raw bq CLI flags appended after the query's own (e.g.
    /// --bq-args "--location=eu --cache=false"); splits on whitespace,
    /// no quotes inside
    #[arg(long)]
    pub(crate) bq_args: Option<String>,

    /// Kill the gcloud/bq fetcher after this many seconds — a hung child
    /// (a 20-minute silent bq was seen for real) otherwise blocks forever;
    /// its job id stays in stderr for later retrieval
    #[arg(long)]
    pub(crate) timeout: Option<u64>,

    /// Keep only the biggest N sessions (event count) in trace mode —
    /// full or summary; the rest drop with a note. With hundreds of IDs
    /// even the compact map overflows context.
    #[arg(long)]
    pub(crate) top: Option<usize>,

    /// Cut any field (or whole line without tabs) after N chars with an
    /// "…" marker — huge XML bodies don't need SQL SUBSTR hacks
    #[arg(long)]
    pub(crate) max_field_len: Option<usize>,

    /// Path to TOML config file (timestamp_regex, boilerplate_regex, default_trace)
    #[arg(long)]
    pub(crate) config: Option<String>,

    /// Skip the default agent pipeline: verbatim lines — no trim of
    /// timestamps or boilerplate, no dedup, no cap. Everything the
    /// source gave, exactly as given.
    #[arg(long)]
    pub(crate) raw: bool,

    /// Run as an MCP stdio server exposing this binary as agent tools
    /// (the CLI stays the single contract; same release = no flag skew)
    #[arg(long)]
    pub(crate) mcp: bool,

    /// Print an agent-facing markdown card (what cardio is, flags, output,
    /// time gotchas) and exit — pipe it: `cardio --agent-md > AGENTS.md`
    #[arg(long)]
    pub(crate) agent_md: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CardConfig {
    timestamp_regex: Option<String>,
    boilerplate_regex: Option<String>,
    default_trace: Option<String>,
}

fn compile_regex(pattern: &str, source: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| {
        eprintln!("Invalid regex {} {:?}: {}", source, pattern, e);
        exit(1);
    })
}

/// --stats folds digit runs to '#' before counting: "sid=123" and
/// "sid=456" are ONE pattern to the survey, not two (three agents hit
/// per-UUID floods that told them nothing). Dedup keeps exact lines.
static DIGIT_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new("[0-9]+").unwrap());

/// Stats mode: survey the whole file; returns counts + whether capped.
fn run_stats(proc: &mut Pipeline) -> (HashMap<String, usize>, bool) {
    let mut stats_map: HashMap<String, usize> = HashMap::new();
    let mut key_buf = String::new(); // reused fold buffer, read via `key` alias
    let mut capped = false;
    while let Some((_, line)) = proc.next_trimmed() {
        // get_mut fast path — no String alloc for a line already seen.
        // Cap gates only new insertions; existing counters keep advancing.
        // Cap the key (--max-field-len) before folding: stats entries hold
        // whole lines, and the cap is what keeps huge bodies bounded here.
        let line = proc.clamp_fields(&line);
        let fold = line.bytes().any(|b| b.is_ascii_digit());
        let key: &str = if fold {
            key_buf.clear();
            key_buf.push_str(&DIGIT_RUN.replace_all(&line, "#"));
            &key_buf
        } else {
            &line
        };
        if let Some(c) = stats_map.get_mut(key) {
            *c += 1;
        } else if stats_map.len() < MAX_STATS_PATTERNS {
            stats_map.insert(key.to_string(), 1);
        } else {
            capped = true;
        }
    }
    (stats_map, capped)
}

fn main() {
    let args = Args::parse();

    // Pure-print flag, no source required — an agent meeting cardio for
    // the first time runs this itself; keeping it a const means the card
    // can never drift from the binary's real flags.
    if args.agent_md {
        print!("{}", agent_md::AGENT_MD);
        exit(0);
    }

    // MCP stdio server mode: this binary as agent tools (line protocol).
    // stdout becomes the JSON-RPC channel, so nothing else may write it.
    if args.mcp {
        mcp::serve();
        exit(0);
    }

    // Lean is the default pipeline: unless --raw, dedup on and the cap
    // defaults to LEAN_MAX_LINES (an explicit --max-lines wins --raw
    // notwithstanding: asking for a cap is asking for a cap). Mutating
    // args keeps the runners untouched; a local dedup so the trace-mode
    // warning only fires when --dedup was explicitly passed.
    let mut args = args;
    let dedup = args.dedup || !args.raw;
    if !args.raw {
        args.max_lines = args.max_lines.or(Some(LEAN_MAX_LINES));
    }

    // exactly one source: file / gcloud / bq
    let sources = [
        args.gcloud.is_some(),
        args.bq.is_some(),
        args.file_path.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if sources > 1 {
        eprintln!("Error: pass only one source: a file path, --gcloud, or --bq.");
        exit(1);
    }

    // Fail fast on unusable time bounds: warn-and-stream would pour an
    // unfiltered GB log (or pushdown) out over one stderr line. exit 2 =
    // usage error, distinct from read/write failures.
    for (label, bound) in [("since", &args.since), ("until", &args.until)] {
        if let Some(b) = bound
            && !is_full_iso(b) {
                eprintln!(
                    "Error: --{} {:?} is not a full ISO date (YYYY-MM-DD or with time).",
                    label, b
                );
                exit(2);
            }
    }

    // Flags that only make sense with --bq: usage errors, not silent no-ops
    let bq_only: [(&'static str, bool); 4] = [
        ("--dry-run", args.dry_run),
        ("--max-bytes-billed", args.max_bytes_billed.is_some()),
        ("--project", args.bq_project.is_some()),
        ("--bq-args", args.bq_args.is_some()),
    ];
    if args.bq.is_none()
        && let Some((flag, _)) = bq_only.into_iter().find(|(_, used)| *used) {
            eprintln!("Error: {flag} only works with --bq.");
            exit(2);
        }
    if args.timeout.is_some() && args.bq.is_none() && args.gcloud.is_none() {
        eprintln!("Error: --timeout only works with --bq or --gcloud.");
        exit(2);
    }

    let config = args.config.as_ref().map(|p| {
        let s = std::fs::read_to_string(p).unwrap_or_else(|e| {
            eprintln!("Error reading config {}: {}", p, e);
            exit(1);
        });
        toml::from_str::<CardConfig>(&s).unwrap_or_else(|e| {
            eprintln!("Error parsing config {}: {}", p, e);
            exit(1);
        })
    }).unwrap_or_default();

    // Compile ALL regexes BEFORE spawning gcloud: a bad pattern must not
    // orphan a running child process.
    let default_ts =
        format!(r"(?:{})|(?:[0-9]{{2}}/[A-Za-z]{{3}}/[0-9]{{4}}:[0-9]{{2}}:[0-9]{{2}}:[0-9]{{2}})", *TRIM_BODY);
    let re_timestamp = compile_regex(
        config.timestamp_regex.as_deref().unwrap_or(&default_ts),
        "config timestamp_regex",
    );
    let boilerplate = config.boilerplate_regex.as_deref().unwrap_or(r"(?i)\b(INFO|DEBUG|TRACE)\b");
    // combined strip = one regex pass per line (ts | boilerplate → "")
    let re_trim = compile_regex(
        &format!("(?:{})|({})", default_ts, boilerplate),
        "trim (timestamp | boilerplate)",
    );
    let trace_re = args.trace.as_ref().or(config.default_trace.as_ref())
        .map(|p| {
            let source = if args.trace.is_some() { "--trace" } else { "config default_trace" };
            compile_regex(p, source)
        });

    // Mode precedence: stats is survey (trace/summary ignored); trace/summary
    // need an ID pattern; plain stream otherwise. Dedup applies to stream only.
    let run = if args.stats {
        Run::Stats
    } else if trace_re.is_some() {
        Run::Sessions
    } else {
        Run::Stream
    };

    if args.summary && trace_re.is_none() {
        eprintln!("Error: --summary needs an ID pattern (--trace or config default_trace).");
        exit(1);
    }
    if args.stats && trace_re.is_some() {
        eprintln!("Note: --stats is survey mode; --trace/--summary ignored.");
    }
    if args.dedup && trace_re.is_some() {
        eprintln!("Warning: --dedup is ignored in trace mode (sessions keep every event).");
    }
    if args.tail.is_some() && !matches!(run, Run::Stream) {
        eprintln!("Note: --tail applies to the line stream; --stats/--trace/--summary ignore it.");
    }

    // Bounds are fixed for the whole run: validate (fail-fast above) +
    // canonicalize once, not per line. Date-only bounds grow day-start
    // (since) / next-day (until) times so the end day isn't excluded
    // wholesale; spaces become 'T'.
    let since_valid: Option<String> = args.since.as_ref()
        .map(|s| canonical_bound(s, false));
    let until_valid: Option<String> = args.until.as_ref()
        .map(|u| canonical_bound(u, true));
    let range_filter = since_valid.is_some() || until_valid.is_some();

    let (reader, fetched_child) = make_reader(&args, &since_valid, &until_valid);
    let child_tool = fetched_child.as_ref().map(|(t, _)| *t);
    // Shared with the watchdog thread: main blocks on reads (not on the
    // mutex), so the watchdog can lock freely and kill a hung child.
    let child = fetched_child.map(|(_, c)| std::sync::Arc::new(std::sync::Mutex::new(c)));
    if let (Some(secs), Some(arc)) = (args.timeout, &child) {
        let watchdog = std::sync::Arc::clone(arc);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            let Ok(mut c) = watchdog.lock() else { return };
            match c.try_wait() {
                // reaped or gone: nothing to kill (kill on a reaped pid could
                // hit an unrelated process)
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => {
                    crate::source::kill_fetcher(&mut c);
                    eprintln!("Error: fetcher killed after --timeout {}s (its job id is visible in the output above).", secs);
                }
            }
        });
    }

    let mut proc = Pipeline {
        source: reader,
        re_timestamp,
        re_trim,
        query: args.query.clone(),
        range_filter,
        child_tool,
        verbatim: args.raw,
        max_field_len: args.max_field_len,
        since_valid,
        until_valid,
        ts_norm: String::new(),
        warned_non_iso: false,
        read_error: None,
        early_cut: false,
        empty_entries: 0,
        line_no: 0,
        raw_buf: Vec::new(),
    };

    // --stats/--trace never stream per line, so bloom only exists in stream mode
    let mut bloom = if matches!(run, Run::Stream) && dedup {
        Some(Bloom::new_for_fp_rate(10_000_000, 0.01).expect("Failed to init bloom filter"))
    } else {
        None
    };

    let mut out = io::stdout().lock();
    let mut outbuf: Sink = io::BufWriter::new(&mut out);

    let result = match run {
        Run::Stats => {
            let (map, capped) = run_stats(&mut proc);
            RunResult {
                stats_map: Some(map),
                sessions: None,
                stats_capped: capped,
                sessions_capped: false,
                session_lines_capped: false,
                dedup_inserts: 0,
                dropped_dups: 0,
                window_full: false,
            }
        }
        Run::Sessions => {
            let (map, sessions_capped, session_lines_capped) =
                sessions::collect_sessions(&mut proc, &args, trace_re.as_ref().unwrap());
            RunResult {
                stats_map: None,
                sessions: Some(map),
                stats_capped: false,
                sessions_capped,
                session_lines_capped,
                dedup_inserts: 0,
                dropped_dups: 0,
                window_full: false,
            }
        }
        Run::Stream => {
            let (dedup_inserts, dropped_dups, window_full) = run_stream(&mut proc, bloom.as_mut(), &args, &mut outbuf);
            RunResult {
                stats_map: None,
                sessions: None,
                stats_capped: false,
                sessions_capped: false,
                session_lines_capped: false,
                dedup_inserts,
                dropped_dups,
                window_full,
            }
        }
    };

    let stopped_at_line = proc.line_no;
    let early_cut = proc.early_cut;
    let empty_entries = proc.empty_entries;
    let read_error = proc.read_error.take();
    if let Some(ref e) = read_error {
        eprintln!("Warning: stopped after input line {}: {} (results collected so far are emitted)", stopped_at_line, e);
    }
    // Dropping the pipeline closes the read end: a still-streaming gcloud
    // gets EPIPE and exits instead of deadlocking the wait() below.
    drop(proc);

    flush(&mut outbuf);

    // Honest stderr signals: the default pipeline drops and caps, so the
    // agent (or the person reading the log) must SEE exactly what was
    // given up — never silently.
    if result.dropped_dups > 0 {
        eprintln!(
            "Note: dedup dropped {} duplicate-suspect line{} (a few may actually be unique — --raw keeps everything).",
            result.dropped_dups,
            if result.dropped_dups == 1 { "" } else { "s" }
        );
    }
    if result.window_full && args.tail.is_none() {
        eprintln!(
            "Note: kept the last {} lines — the most recent; the rest of the input was read and trimmed (--raw removes the cap; --query TEXT narrows before reading).",
            args.max_lines.unwrap_or(0)
        );
    }
    if early_cut {
        eprintln!("Note: output stopped at the --max-lines cap; the rest of the input was not read (--raw removes the cap).");
    }
    if result.dedup_inserts > 10_000_000 {
        eprintln!("Warning: --dedup exceeded 10M unique lines; bloom filter is saturated and unique lines may be dropped as false duplicates.");
    }
    if let Some(tool) = child_tool
        && empty_entries > 0 {
            eprintln!("Note: {} empty entr{} from {} were skipped.", empty_entries, if empty_entries == 1 { "y" } else { "ies" }, tool);
        }
    // Real-agent feedback: an empty result looked like a failure because
    // nothing said otherwise. EVERY fetcher run states its volume — zero
    // included — so 0 rows has a witness; the job id is already in stderr
    // from the child.
    if let Some(tool) = child_tool {
        eprintln!("Note: {} streamed {} line{}.", tool, stopped_at_line, if stopped_at_line == 1 { "" } else { "s" });
    }
    if result.stats_capped {
        eprintln!("Warning: --stats reached its {} unique-pattern cap; later patterns untracked (existing counts are complete).", MAX_STATS_PATTERNS);
    }
    if result.sessions_capped {
        eprintln!("Warning: trace session cap ({}) reached; later sessions not collected.", sessions::MAX_TRACE_SESSIONS);
    }
    if result.session_lines_capped {
        eprintln!("Warning: some sessions hit the {}-line cap; their tails were cut.", sessions::MAX_LINES_PER_SESSION);
    }

    if let Some(map) = result.stats_map {
        // Count desc, then pattern asc = deterministic ties
        let mut sorted: Vec<_> = map.into_iter().collect();
        let cmp = |a: &(String, usize), b: &(String, usize)| b.1.cmp(&a.1).then(a.0.cmp(&b.0));
        // ponytail: O(n) top-20 partition, full sort only on the 20 printed
        if sorted.len() > 20 {
            sorted.select_nth_unstable_by(19, cmp);
        }
        sorted.truncate(20);
        sorted.sort_by(cmp);
        if args.json {
            // Machine-readable survey: NDJSON on stdout, the agent's
            // channel. Same top-20 + deterministic tie order as the
            // stderr table.
            for (p, c) in &sorted {
                pipeline::emit(&mut outbuf, &serde_json::json!({"pattern": p, "count": c}).to_string());
            }
        } else {
            eprintln!("\n=== LOG PATTERN STATISTICS ===");
            for (p, c) in &sorted {
                eprintln!("{:>7} | {}", c, p);
            }
            eprintln!("==============================");
        }
    }

    if let Some(sessions_map) = result.sessions {
        let dropped = emit_sessions(&sessions_map, args.top, args.json, &mut outbuf);
        // text mode already carries the count inside its output header;
        // the json stream must stay clean — this note is its witness only
        if args.json && dropped > 0 {
            eprintln!(
                "Note: {} smaller session{} dropped (--top {}).",
                dropped, if dropped == 1 { "" } else { "s" }, args.top.unwrap_or(0)
            );
        }
    }
    flush(&mut outbuf);

    // We cut the stream early (--max-lines / --timeout): kill the fetcher
    // outright. Rust ignores SIGPIPE and children inherit it, so gcloud
    // would die on EPIPE with a nonzero exit — reported as a failure we
    // caused.
    if early_cut
        && let Some(child) = &child {
            let mut g = child.lock().unwrap();
            source::kill_fetcher(&mut g);
        }
    if let Some(child) = &child {
        // Poll try_wait with the mutex held per iteration only: holding it
        // across a blocking wait() would deadlock the --timeout watchdog
        // in exactly the hang cases it exists for (child alive, stdout
        // closed, main parked in wait()).
        let status;
        loop {
            // scoped block: the MutexGuard must NOT outlive try_wait — a
            // guard held across the sleep arm would keep the watchdog
            // waiting on the lock forever (match temporaries live until
            // the arms complete, so a naked match scrutinee deadlocks)
            let probed = { child.lock().unwrap().try_wait() };
            match probed {
                Ok(Some(s)) => { status = Some(s); break; }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => {
                    eprintln!("Warning: failed to wait for {}: {}", child_tool.unwrap_or("fetcher"), e);
                    status = None;
                    break;
                }
            }
        }
        let status = status.unwrap();
        if !status.success() {
            if early_cut {
                // already reported above; ignore the failure we caused
                // (EPIPE from killing the fetcher)
            } else {
                // Propagate: scripts piping cardio must see fetch failures
                eprintln!("Error: {} exited with {}", child_tool.unwrap_or("fetcher"), status);
                exit(1);
            }
        }
    }

    if read_error.is_some() {
        // Truncated input is a failure even though we emitted partial results
        exit(1);
    }
}