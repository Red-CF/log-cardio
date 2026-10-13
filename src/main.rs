use regex::Regex;
use std::collections::{btree_map, BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdout, Command, Stdio, exit};
use bloomfilter::Bloom;
use clap::Parser;
use serde::{Serialize, Deserialize};
#[derive(Parser, Debug)]
#[command(author, version, about = "Trim noise from logs for LLM processing")]
struct Args {
    /// Path to the log file. If omitted, reads from stdin or GCloud.
    file_path: Option<String>,

    /// Filter lines containing this text (applied before trimming)
    #[arg(long)]
    query: Option<String>,

    /// Remove duplicate lines
    #[arg(long)]
    dedup: bool,

    /// Max number of lines to output (event count in trace mode; ignored with --stats)
    #[arg(long)]
    max_lines: Option<usize>,

    /// Output in JSON format
    #[arg(long)]
    json: bool,

    /// Only lines with timestamp >= this (ISO, or YYYY-MM-DD = day start).
    #[arg(long)]
    since: Option<String>,

    /// Only lines with timestamp < this (ISO, or YYYY-MM-DD = next-day start).
    #[arg(long)]
    until: Option<String>,

    /// Group logs by a captured ID (e.g., "request_id=([a-z0-9-]+)")
    #[arg(long)]
    trace: Option<String>,

    /// Compact per-ID map (event count + first/last event, IDs preserved)
    /// instead of full sessions; needs --trace or config default_trace
    #[arg(long)]
    summary: bool,

    /// Survey mode: frequency summary of the whole file to stderr, no stream output
    #[arg(long)]
    stats: bool,

    /// GCloud Logging filter (e.g. "resource.type=gce_instance")
    #[arg(long)]
    gcloud: Option<String>,

    /// Path to TOML config file (timestamp_regex, boilerplate_regex, default_trace)
    #[arg(long)]
    config: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct CardConfig {
    timestamp_regex: Option<String>,
    boilerplate_regex: Option<String>,
    default_trace: Option<String>,
}

#[derive(Serialize)]
struct LogEntry {
    index: usize,
    content: String,
}

fn compile_regex(pattern: &str, source: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| {
        eprintln!("Invalid regex {} {:?}: {}", source, pattern, e);
        exit(1);
    })
}

/// ISO-8601 date prefix (YYYY-MM-DD...) — only these sorts are safe for
/// lexicographic range compare (string order == chronological order)
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 4 && b[..4].iter().all(u8::is_ascii_digit) && b.get(4) == Some(&b'-')
}

fn is_full_iso(s: &str) -> bool {
    if !is_iso_date(s) || s.len() < 10 { return false; }
    let b = s.as_bytes();
    if b[7] != b'-' || ![5, 6, 8, 9].iter().all(|i| u8::is_ascii_digit(&b[*i])) {
        return false;
    }
    // ponytail: tail chars restricted — bounds are embedded verbatim in the
    // gcloud GQL filter, so a stray quote or letter would corrupt the query.
    b[10..].iter().all(|c| {
        u8::is_ascii_digit(c) || matches!(c, b':' | b'T' | b'Z' | b'.' | b'+' | b'-' | b' ')
    })
}

/// Broken pipe (consumer left: head, grep -q) exits quietly; any other write
/// failure (ENOSPC etc.) is fatal — callers exit 1 so truncation is noticed.
fn on_write_err(e: io::Error, what: &str) {
    if e.kind() == io::ErrorKind::BrokenPipe {
        exit(0);
    }
    eprintln!("Error {}: {}", what, e);
}

fn emit<W: Write>(w: &mut W, text: &str) {
    if let Err(e) = writeln!(w, "{}", text) {
        on_write_err(e, "writing output");
        exit(1);
    }
}

/// Normalize "YYYY-MM-DD HH:MM:SS" to "T" separator so lexicographic compare
/// against a "T"-separated bound keeps time order (' ' < 'T' would mis-order).
/// Result is written into `buf` (reused per iteration) and returned as &str.
fn normalize_iso<'a>(ts: &'a str, buf: &'a mut String) -> &'a str {
    if ts.len() > 10 && ts.as_bytes()[10] == b' ' {
        buf.clear();
        buf.push_str(&ts[..10]);
        buf.push('T');
        buf.push_str(&ts[11..]);
        buf.as_str()
    } else {
        ts
    }
}

/// Canonicalize a validated --since bound: date-only grows a day-start time.
/// ponytail: bounds normalized at this layer so callers can't forget the
/// convention; offset handling stays tz-naive — chrono if that ever matters.
fn canonical_since(s: &str) -> String {
    if s.len() == 10 { format!("{}T00:00:00", s) } else { s.replacen(' ', "T", 1) }
}

/// --until is <, so a date-only bound means NEXT-day start: "--until 2026-10-09"
/// keeps all of the 9th instead of excluding the whole end day.
fn canonical_until(s: &str) -> String {
    if s.len() == 10 { next_day_start(s) } else { s.replacen(' ', "T", 1) }
}

/// One date increment, no chrono: manual month/leap arithmetic.
fn next_day_start(date: &str) -> String {
    let (mut y, mut m, mut d) = (
        date[..4].parse::<u16>().unwrap_or(0),
        date[5..7].parse::<u8>().unwrap_or(12),
        date[8..10].parse::<u8>().unwrap_or(31),
    );
    d += 1;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim: u8 = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        2 if leap => 29,
        2 => 28,
        _ => 30,
    };
    if d > dim {
        d = 1;
        m += 1;
        if m > 12 { m = 1; y += 1; }
    }
    format!("{:04}-{:02}-{:02}T00:00:00", y, m, d)
}

fn main() {
    let args = Args::parse();

    if args.gcloud.is_some() && args.file_path.is_some() {
        eprintln!("Error: pass either a file path or --gcloud, not both.");
        exit(1);
    }

    // Single warning loop over both time bounds: unusable bounds must be loud
    for (label, bound) in [("since", &args.since), ("until", &args.until)] {
        if let Some(b) = bound {
            if !is_full_iso(b) {
                eprintln!(
                    "Warning: --{} {:?} is not a full ISO date (YYYY-MM-DD or with time); the filter may be a no-op.",
                    label, b
                );
            }
        }
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

    // Compile ALL regexes BEFORE spawning gcloud: a bad pattern must not orphan
    // a running child process.
    let re_timestamp = compile_regex(
        config.timestamp_regex.as_deref().unwrap_or(
            // ponytail: offsets matched so trimming strips them fully; the
            // lexicographic compare stays tz-naive — chrono if that matters
            r"(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)|(\d{2}/\w{3}/\d{4}:\d{2}:\d{2}:\d{2})"
        ),
        "config timestamp_regex",
    );
    let re_boilerplate = compile_regex(
        config.boilerplate_regex.as_deref().unwrap_or(r"(?i)\b(INFO|DEBUG|TRACE)\b"),
        "config boilerplate_regex",
    );
    let trace_re = args.trace.as_ref().or(config.default_trace.as_ref())
        .map(|p| {
            let source = if args.trace.is_some() { "--trace" } else { "config default_trace" };
            compile_regex(p, source)
        });
    let trace_mode = trace_re.is_some();
    let summary_mode = args.summary && trace_mode;
    if args.summary && !trace_mode && !args.stats {
        eprintln!("Error: --summary needs an ID pattern (--trace or config default_trace).");
        exit(1);
    }

    if args.dedup && trace_mode {
        eprintln!("Warning: --dedup is ignored in trace mode (sessions keep every event).");
    }
    if args.stats && trace_mode {
        eprintln!("Note: --stats is survey mode; --trace/--summary ignored.");
    }

    // Bounds are fixed for the whole run: validate + canonicalize once, not
    // per line. Date-only bounds grow day-start (since / next-day (until)
    // times so the end day isn't excluded wholesale; spaces become 'T'.
    let since_valid: Option<String> = args.since.as_ref()
        .filter(|s| is_full_iso(s)).map(|s| canonical_since(s));
    let until_valid: Option<String> = args.until.as_ref()
        .filter(|u| is_full_iso(u)).map(|u| canonical_until(u));
    let range_filter = since_valid.is_some() || until_valid.is_some();

    // Reader: gcloud child process, file, or stdin.
    let mut gc_child: Option<Child> = None;
    let mut reader: Box<dyn BufRead> = if let Some(filter) = &args.gcloud {
        // Push validated time bounds into the SERVER-side filter: they are
        // valid ISO by is_full_iso, so embed as timestamp clauses to avoid
        // downloading months of entries that local filtering would drop.
        let mut gc_filter = filter.clone();
        // Bounds already passed normalize_iso: no space separator remains
        if let Some(s) = &since_valid {
            gc_filter.push_str(&format!(" AND timestamp>=\"{}\"", s));
        }
        if let Some(u) = &until_valid {
            gc_filter.push_str(&format!(" AND timestamp<\"{}\"", u));
        }
        let gc_args = vec![
            "logging".to_string(), "read".to_string(), gc_filter,
            "--format".into(), "value(timestamp,textPayload,jsonPayload.message)".into(),
            "--order=asc".into(),
        ];
        // ponytail: no --limit pushdown for --max-lines; the early-cut kill
        // already bounds the download.
        let mut child = Command::new("gcloud")
            .args(&gc_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| {
                eprintln!("Failed to execute gcloud: {}. Is gcloud CLI installed?", e);
                exit(1);
            });
        let stdout: ChildStdout = child.stdout.take().expect("gcloud stdout");
        gc_child = Some(child);
        Box::new(BufReader::new(stdout))
    } else if let Some(path) = &args.file_path {
        let file = File::open(path).unwrap_or_else(|e| {
            eprintln!("Error opening file {}: {}", path, e);
            exit(1);
        });
        Box::new(BufReader::new(file))
    } else {
        Box::new(BufReader::new(io::stdin()))
    };

    // BTreeMap = stable, reproducible session order across runs
    let mut trace_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // summary mode: (event count, first, last) — IDs always preserved
    let mut summary_map: BTreeMap<String, (usize, String, String)> = BTreeMap::new();

    let mut stats_map: HashMap<String, usize> = HashMap::new();
    const MAX_STATS_PATTERNS: usize = 100_000;
    const MAX_TRACE_SESSIONS: usize = 10_000;
    const MAX_LINES_PER_SESSION: usize = 1_000;

    let mut sessions_capped = false;
    let mut session_lines_capped = false;
    let mut stats_capped = false;

    // --stats counts before dedup, so bloom has no effect there: skip the
    // 10M-item filter and its per-line hashing entirely
    let mut bloom = if args.dedup && !trace_mode && !args.stats {
        Some(Bloom::new_for_fp_rate(10_000_000, 0.01).expect("Failed to init bloom filter"))
    } else {
        None
    };
    let mut count = 0;
    let mut ts_norm = String::new();
    let mut warned_non_iso = false;
    let mut dedup_inserts = 0usize;
    let mut empty_entries = 0usize;

    let mut out = io::stdout().lock();
    let mut outbuf = BufWriter::new(&mut out);

    // read_until (bytes) + lossy decode: one invalid UTF-8 byte degrades to
    // U+FFFD on that line instead of erroring and truncating the stream.
    let mut raw_buf: Vec<u8> = Vec::new();
    let mut line_no = 0usize;
    let mut read_error: Option<io::Error> = None;
    let mut early_cut = false;
    loop {
        raw_buf.clear();
        match reader.read_until(b'\n', &mut raw_buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                read_error = Some(e);
                break;
            }
        }
        line_no += 1;

        // max_lines caps output/trace collection; --stats surveys the whole file
        if let Some(max) = args.max_lines {
            if !args.stats && count >= max {
                early_cut = true;
                break;
            }
        }

        let line = String::from_utf8_lossy(&raw_buf);
        let line = line.trim_end_matches(['\n', '\r']);

        // gcloud entries with neither textPayload nor jsonPayload.message
        // render as blank lines: count them so vanished records are visible
        if gc_child.is_some() && line.trim().is_empty() {
            empty_entries += 1;
        }

        // Fast rejects FIRST, before any allocation
        if let Some(ref q) = args.query { if !line.contains(q) { continue; } }

        // Time range: the line's FIRST timestamp match, wherever it sits
        // (brackets/level prefixes are common), and it must be ISO — Apache
        // format ("09/Oct/2026:...") also matches but sorts backwards
        // lexicographically. ponytail: ts fields come first in real formats,
        // so first-match is the intended ts; ISO strings sort chronologically
        // and space separators are normalized to 'T' for the compare.
        if range_filter {
            let lead = re_timestamp.find(line)
                .filter(|m| is_full_iso(m.as_str()))
                .map(|m| m.as_str());
            if let Some(ts) = lead {
                let ts = normalize_iso(ts, &mut ts_norm);
                if let Some(s) = &since_valid {
                    if ts < s.as_str() { continue; }
                }
                if let Some(u) = &until_valid {
                    if ts >= u.as_str() { continue; }
                }
            } else if !warned_non_iso && re_timestamp.is_match(line) {
                warned_non_iso = true;
                eprintln!("Warning: --since/--until needs a leading ISO (YYYY-...) timestamp; lines with non-ISO leading timestamps bypass the range filter.");
            }
        }

        let trimmed = re_timestamp.replace_all(line, "");
        let trimmed = re_boilerplate.replace_all(&trimmed, "");
        let final_line = trimmed.trim();
        if final_line.is_empty() { continue; }

        // Stats: get_mut fast path — no String alloc for a line already seen.
        // Cap gates only new insertions; existing counters keep advancing.
        if args.stats {
            if let Some(c) = stats_map.get_mut(final_line) {
                *c += 1;
            } else if stats_map.len() < MAX_STATS_PATTERNS {
                stats_map.insert(final_line.to_string(), 1);
            } else {
                stats_capped = true;
            }
        }

        // Trace: collect BEFORE dedup (session integrity); single entry probe.
        // --stats is survey mode: sessions are never emitted, don't collect.
        if !args.stats && let Some(ref re) = trace_re {
            match re.captures(line) {
                Some(caps) => {
                    // group 0 is always present when captures() succeeds
                    let id = caps.get(1)
                        .unwrap_or_else(|| caps.get(0).unwrap())
                        .as_str().to_string();
                    if summary_mode {
                        // summary keeps per-ID count + first/last event only
                        let len_before = summary_map.len();
                        match summary_map.entry(id) {
                            btree_map::Entry::Occupied(mut e) => {
                                let s = e.get_mut();
                                s.0 += 1;
                                s.2 = final_line.to_string();
                                count += 1;
                            }
                            btree_map::Entry::Vacant(e) => {
                                if len_before < MAX_TRACE_SESSIONS {
                                    e.insert((1, final_line.to_string(), final_line.to_string()));
                                    count += 1;
                                } else {
                                    sessions_capped = true;
                                }
                            }
                        }
                        continue;
                    }
                    let len_before = trace_map.len();
                    match trace_map.entry(id) {
                        btree_map::Entry::Occupied(mut e) => {
                            let session = e.get_mut();
                            if session.len() < MAX_LINES_PER_SESSION {
                                session.push(final_line.to_string());
                                count += 1;
                            } else {
                                session_lines_capped = true;
                            }
                        }
                        btree_map::Entry::Vacant(e) => {
                            if len_before < MAX_TRACE_SESSIONS {
                                e.insert(vec![final_line.to_string()]);
                                count += 1;
                            } else {
                                sessions_capped = true;
                            }
                        }
                    }
                }
                None => continue,
            }
        }

        if let Some(ref mut b) = bloom {
            // single probe: check + insert in one hash pass
            if b.check_and_set(final_line) { continue; }
            // Track actual insertions: saturating past capacity = silent data loss
            dedup_inserts += 1;
        }

        // Stream output: trace mode emits sessions at the end, stats none;
        // otherwise one line per event
        if !trace_mode && !args.stats {
            if args.json {
                // plain usize/String struct — serialization cannot fail
                let json = serde_json::to_string(&LogEntry {
                    index: line_no,
                    content: final_line.to_string(),
                }).unwrap();
                emit(&mut outbuf, &json);
            } else {
                emit(&mut outbuf, final_line);
            }
            count += 1;
        }
    }

    if let Some(ref e) = read_error {
        eprintln!("Warning: stopped after input line {}: {} (results collected so far are emitted)", line_no, e);
    }

    // We cut the stream early (--max-lines): kill gcloud outright. rust
    // ignores SIGPIPE and children inherit it, so gcloud would die on EPIPE
    // with a nonzero exit — reported as a failure we caused.
    if early_cut {
        if let Some(child) = gc_child.as_mut() { let _ = child.kill(); }
    }
    // Drop the read end so a still-streaming gcloud gets EPIPE and exits
    // instead of deadlocking wait() below.
    drop(reader);

    let flush = |buf: &mut BufWriter<&mut io::StdoutLock>| {
        if let Err(e) = buf.flush() {
            on_write_err(e, "flushing output");
            exit(1);
        }
    };
    flush(&mut outbuf);

    if dedup_inserts > 10_000_000 {
        eprintln!("Warning: --dedup exceeded 10M unique lines; bloom filter is saturated and unique lines may be dropped as false duplicates.");
    }
    if empty_entries > 0 {
        eprintln!("Note: {} gcloud entr{} had no textPayload/message and were skipped.", empty_entries, if empty_entries == 1 { "y" } else { "ies" });
    }

    if stats_capped {
        eprintln!("Warning: --stats reached its {} unique-pattern cap; later patterns untracked (existing counts are complete).", MAX_STATS_PATTERNS);
    }
    if sessions_capped {
        eprintln!("Warning: trace session cap ({}) reached; later sessions not collected.", MAX_TRACE_SESSIONS);
    }
    if session_lines_capped {
        eprintln!("Warning: some sessions hit the {}-line cap; their tails were cut.", MAX_LINES_PER_SESSION);
    }

    if args.stats {
        // Count desc, then pattern asc = deterministic ties
        let mut sorted: Vec<_> = stats_map.into_iter().collect();
        let cmp = |a: &(String, usize), b: &(String, usize)| b.1.cmp(&a.1).then(a.0.cmp(&b.0));
        // ponytail: O(n) top-20 partition, full sort only on the 20 printed
        if sorted.len() > 20 {
            sorted.select_nth_unstable_by(19, cmp);
        }
        sorted.truncate(20);
        sorted.sort_by(cmp);
        eprintln!("\n=== LOG PATTERN STATISTICS ===");
        for (p, c) in &sorted {
            eprintln!("{:>7} | {}", c, p);
        }
        eprintln!("==============================");
    }

    if summary_mode && !args.stats {
        if args.json {
            for (id, (count, first, last)) in &summary_map {
                let json = serde_json::to_string(&serde_json::json!({
                    "id": id, "count": count, "first": first, "last": last
                })).unwrap();
                emit(&mut outbuf, &json);
            }
        } else {
            emit(&mut outbuf, &format!("\n=== TRACE SUMMARY: {} sessions ===", summary_map.len()));
            for (id, (count, first, last)) in &summary_map {
                // 1-event session: first == last — print once, singular label
                let body = if *count == 1 { first.to_string() } else { format!("{}\n{}", first, last) };
                emit(&mut outbuf, &format!(
                    "--- [{}] {} event{} ---\n{}", id, count,
                    if *count == 1 { "" } else { "s" }, body
                ));
            }
        }
    } else if trace_mode {
        for (id, lines) in trace_map {
            if args.json {
                let json = serde_json::to_string(&serde_json::json!({"id": id, "events": lines}))
                    .unwrap();
                emit(&mut outbuf, &json);
            } else {
                emit(&mut outbuf, &format!("\n--- [{}] ---\n{}", id, lines.join("\n")));
            }
        }
    }
    flush(&mut outbuf);

    if let Some(child) = gc_child.as_mut() {
        match child.wait() {
            Ok(status) if !status.success() => {
                if early_cut {
                    eprintln!("Note: gcloud stream cut early (--max-lines).");
                } else {
                    // Propagate: scripts piping cardio must see gcloud failures
                    eprintln!("Error: gcloud exited with {}", status);
                    exit(1);
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("Warning: failed to wait for gcloud: {}", e),
        }
    }

    if read_error.is_some() {
        // Truncated input is a failure even though we emitted partial results
        exit(1);
    }
}