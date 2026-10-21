//! cardio — trim noise from logs for LLM agents. main.rs is PURE
//! orchestration: parse, validate, wire, call the layer that owns the
//! behavior (pipeline/sessions/source/timestamps/notes/stats), emit the
//! witnesses. New behavior belongs in the layer it affects; the exit
//! contract lives in errors::Fail; tuning knobs in consts.

mod agent_md;
mod consts;
mod errors;
mod mcp;
mod notes;
mod pipeline;
mod sessions;
mod source;
mod stats;
mod timestamps;

use clap::Parser;
use errors::{Fail, bail};
use regex::Regex;
use sessions::Run;
use pipeline::{Pipeline, Sink, flush, run_stream};
use source::{collect_fetcher, make_reader, spawn_watchdog};
use std::io;
use std::process::exit;
use timestamps::{TRIM_BODY, canonical_bound, is_full_iso};

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

    /// Keep only the LAST N kept lines — the most recent beat the old
    /// (whole input reads). The lean default for files/stdin
    #[arg(long)]
    pub(crate) tail: Option<usize>,

    /// Deprecated since 1.0.0: the tail window is --tail; a trace-mode
    /// event cap is --head. Hidden from help; a note says where it moved
    #[arg(long, hide = true)]
    pub(crate) max_lines: Option<usize>,

    /// Keep only the FIRST N kept lines — the lean default for cloud
    /// fetchers (the query's own ordering is user intent); in
    /// --trace/--summary mode this is the event cap per run
    #[arg(long)]
    pub(crate) head: Option<usize>,

    /// Drop the first N kept lines before the window — bq paging:
    /// `--skip 100 --head 100` is page 2
    #[arg(long)]
    pub(crate) skip: Option<usize>,

    /// Output in JSON format
    #[arg(long)]
    pub(crate) json: bool,

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

    /// kubectl logs target: "pod/my-api-abc123", "deployment/my-api" or a
    /// bare name; uses the current kubectl context and namespace. --since
    /// pushes to kubectl's --since-time (UTC); --until filters locally
    #[arg(long)]
    pub(crate) k8s: Option<String>,

    /// Container name for kubectl logs (-c); with --k8s
    #[arg(short = 'c', long = "container")]
    pub(crate) k8s_container: Option<String>,

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

    /// Kill the fetcher after this many seconds (default FETCH_TIMEOUT_
    /// DEFAULT for fetchers; 0 disables) — a hung child (a 20-minute
    /// silent bq was seen for real) otherwise blocks forever; its job id
    /// stays in stderr for later retrieval
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
        bail(Fail::Source(format!("Invalid regex {} {:?}: {}", source, pattern, e)))
    })
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

    // Lean is the default pipeline: unless --raw, dedup on, and the
    // window defaults to LEAN_MAX_LINES with a side chosen per source
    // (fetchers keep their head — a fetcher's query ordering is the
    // caller's intent; streams keep the tail — recent beats old). An
    // explicit --head/--tail wins --raw notwithstanding: asking for a
    // cap is asking for a cap.
    let mut args = args;
    // --max-lines moved in 1.0.0: window role -> --tail, trace-role -> --head.
    // Hidden deprecation alias so existing scripts get one release of hints.
    if let Some(n) = args.max_lines {
        if args.tail.is_none() {
            args.tail = Some(n);
        }
        eprintln!("Note: --max-lines is deprecated since 1.0.0 — use --tail for the window, --head for a trace-mode event cap.");
    }
    args.max_lines = None;
    // explicitness before the window policy fills defaults in below:
    // the stderr notes only narrate DROPS the caller did not ask for
    let explicit_tail = args.tail.is_some();
    let explicit_head = args.head.is_some();
    let dedup = args.dedup || !args.raw;
    if !args.raw {
        args.max_field_len = args.max_field_len.or(Some(consts::FIELD_LEN_DEFAULT));
    }
    if args.max_field_len == Some(0) {
        args.max_field_len = None; // explicit "unlimited fields"
    }
    if args.head.is_some() && args.tail.is_some() {
        bail(Fail::Usage("--head and --tail are both windows; pass at most one.".into()));
    }

    // exactly one source: file / gcloud / bq / k8s
    let sources = [
        args.gcloud.is_some(),
        args.bq.is_some(),
        args.k8s.is_some(),
        args.file_path.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if sources > 1 {
        bail(Fail::Source("pass only one source: a file path, --gcloud, --bq, or --k8s.".into()));
    }

    // Fail fast on unusable time bounds: warn-and-stream would pour an
    // unfiltered GB log (or pushdown) out over one stderr line. exit 2 =
    // usage error, distinct from read/write failures.
    for (label, bound) in [("since", &args.since), ("until", &args.until)] {
        if let Some(b) = bound
            && !is_full_iso(b) {
                bail(Fail::Usage(format!(
                    "--{label} {b:?} is not a full ISO date (YYYY-MM-DD or with time)."
                )));
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
            bail(Fail::Usage(format!("{flag} only works with --bq.")));
        }
    if args.timeout.is_some()
        && args.bq.is_none() && args.gcloud.is_none() && args.k8s.is_none() {
        bail(Fail::Usage("--timeout only works with --bq, --gcloud, or --k8s.".into()));
    }
    if args.k8s_container.is_some() && args.k8s.is_none() {
        bail(Fail::Usage("-c/--container only works with --k8s.".into()));
    }

    let config = args.config.as_ref().map(|p| {
        let s = std::fs::read_to_string(p)
            .unwrap_or_else(|e| bail(Fail::Source(format!("reading config {p}: {e}"))));
        toml::from_str::<CardConfig>(&s)
            .unwrap_or_else(|e| bail(Fail::Source(format!("parsing config {p}: {e}"))))
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
        bail(Fail::Usage("--summary needs an ID pattern (--trace or config default_trace).".into()));
    }
    if args.stats && trace_re.is_some() {
        eprintln!("Note: --stats is survey mode; --trace/--summary ignored.");
    }
    if args.dedup && trace_re.is_some() {
        eprintln!("Warning: --dedup is ignored in trace mode (sessions keep every event).");
    }
    if (args.tail.is_some() || args.head.is_some() || args.skip.is_some())
        && !matches!(run, Run::Stream) {
        eprintln!("Note: --head/--tail/--skip apply to the line stream; --stats ignores them (--head caps events in trace mode).");
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
    // THE window policy, next to the source decision that drives it: lean
    // (no explicit flag, no --raw) caps at LEAN_MAX_LINES — fetchers keep
    // the HEAD (their query's ordering is the caller's intent), streams
    // keep the TAIL (recent beats old). trace-mode event caps share --head.
    if matches!(run, Run::Stream | Run::Sessions)
        && args.head.is_none() && args.tail.is_none()
        && !args.raw && !args.stats {
            if child_tool.is_some() { args.head = Some(consts::LEAN_MAX_LINES); }
            else { args.tail = Some(consts::LEAN_MAX_LINES); }
    }
    // Shared with the watchdog thread: main blocks on reads (not on the
    // mutex), so the watchdog can lock freely and kill a hung child.
    let child = fetched_child.map(|(_, c)| std::sync::Arc::new(std::sync::Mutex::new(c)));
    // fetchers are protected by default: nobody should remember the guard
    if args.timeout.is_none() && child_tool.is_some() {
        args.timeout = Some(consts::FETCH_TIMEOUT_DEFAULT);
    }
    if let (Some(secs), Some(arc)) = (args.timeout, &child) {
        spawn_watchdog(arc, secs);
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

    let mut out = io::stdout().lock();
    let mut outbuf: Sink = io::BufWriter::new(&mut out);

    let result = match run {
        Run::Stats => {
            let (map, capped) = stats::run_stats(&mut proc);
            sessions::RunResult {
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
            sessions::RunResult {
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
            let (dedup_inserts, dropped_dups, window_full) = run_stream(&mut proc, dedup, &args, &mut outbuf);
            sessions::RunResult {
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

    let sessions_top_dropped = if let Some(sessions_map) = &result.sessions {
        let dropped = sessions::emit_sessions(sessions_map, args.top, args.json, &mut outbuf);
        flush(&mut outbuf);
        dropped
    } else {
        0
    };

    notes::emit(
        &notes::Notes {
            explicit_tail,
            explicit_head,
            head_size: args.head.unwrap_or(consts::LEAN_MAX_LINES),
            child_tool,
            early_cut,
            stopped_at_line,
            empty_entries,
        },
        &result,
        sessions_top_dropped,
        args.json,
        args.top.unwrap_or(0),
    );

    if let Some(map) = result.stats_map {
        if args.json {
            stats::print_json(&map, &mut outbuf);
        } else {
            stats::print_table(&map);
        }
    }
    flush(&mut outbuf);

    // Fetch failures propagate: scripts piping cardio must see them
    // (an early-cut kill is OUR failure, already reported — ignored).
    if let Some(child) = &child
        && let Err(f) = collect_fetcher(child, child_tool, early_cut) {
            bail(f);
        }

    if read_error.is_some() {
        // Truncated input is a failure even though we emitted partial results
        exit(1);
    }
}