use regex::Regex;
use std::collections::{btree_map, BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdout, Command, Stdio, exit};
use bloomfilter::Bloom;
use clap::Parser;
use serde::{Serialize, Deserialize};
use std::sync::LazyLock;

const MAX_STATS_PATTERNS: usize = 100_000;
const MAX_TRACE_SESSIONS: usize = 10_000;
const MAX_LINES_PER_SESSION: usize = 1_000;

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

    /// BigQuery SQL — runs the bq CLI (CSV output) and feeds rows into the
    /// same pipeline (query/range/trace/summary/stats)
    #[arg(long)]
    bq: Option<String>,

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

/// Full ISO structure: date, optional time (hh:mm(:ss)?)?, optional
/// fractional seconds (SECONDS only — a frac on minutes is not a real
/// log grammar and is rejected like any other invalid ts) + tz offset.
/// ponytail: structural + calendar-sanity only — exact days-in-month
/// and tz semantics stay out (chrono if needed).
/// [0-9] over \d on purpose: regex \d is Unicode-aware and every byte
/// check downstream (is_full_iso arithmetic, all_digits, truncate,
/// shape gate) assumes ASCII digits — pinning the range retires the
/// whole unicode-digit class at the grammar instead of netting it
/// branch by branch.
static FULL_ISO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}([ T][0-9]{2}:[0-9]{2}(?::[0-9]{2}(?:\.[0-9]+)?)?(?:Z|[+-][0-9]{2}:?[0-9]{2})?)?$")
        .unwrap()
});

fn is_full_iso(s: &str) -> bool {
    if !FULL_ISO_RE.is_match(s) {
        return false;
    }
    // calendar sanity: month 1-12, day 1-31 (catches 2026-99-99, Feb 99);
    // with a time present, hour <= 24 (only exactly 24:00(:00) passes, via
    // is_eod_24 which to_canonical's rewrite shares — no drift), minute
    // <= 59, second <= 59 (T99:99:99 would mis-order locally and be
    // unembeddable server-side; leap seconds :60 are rejected too)
    // ponytail: byte checks at fixed offsets — keep in sync with
    // FULL_ISO_RE above; capture-group parsing would stay safe under
    // regex relaxations but costs on this hot path
    let b = s.as_bytes();
    let m = ((b[5] - b'0') as u32) * 10 + (b[6] - b'0') as u32;
    let d = ((b[8] - b'0') as u32) * 10 + (b[9] - b'0') as u32;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return false;
    }
    if b.len() > 10 {
        // FULL_ISO_RE only allows a time tail as [ T]HH:MM(:SS...)? — an
        // offset/UTC suffix starts at/before byte 13 as ':'; if bytes 11-15
        // are HH:MM then any tz tail begins after byte 16
        let h = ((b[11] - b'0') as u32) * 10 + (b[12] - b'0') as u32;
        if h > 24 {
            return false;
        }
        if h == 24 && !is_eod_24(b) {
            // the shape rule lives in is_eod_24 — to_canonical's rewrite
            // accepts exactly what passes here (no second copy). What a
            // rejected 24:xx / :60 LINE means downstream: no valid lead ts
            // in the line -> it BYPASSES the range filter with the usual
            // one-shot "non-ISO" warning (so a bad-hour line can leak past
            // bounds; bounds themselves fail fast above).
            return false;
        }
        let mi = ((b[14] - b'0') as u32) * 10 + (b[15] - b'0') as u32;
        if h <= 23 && !(0..=59).contains(&mi) {
            return false;
        }
        if h <= 23 && b.len() >= 19 && b[16] == b':' {
            // h == 24 needs nothing here: is_eod_24 already forced "00"
            let sec = ((b[17] - b'0') as u32) * 10 + (b[18] - b'0') as u32;
            if sec > 59 {
                return false;
            }
        }
    }
    true
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

/// Output sink shared by the mode runners.
type Sink<'w> = BufWriter<&'w mut io::StdoutLock<'static>>;

fn flush(outbuf: &mut Sink) {
    if let Err(e) = outbuf.flush() {
        on_write_err(e, "flushing output");
        exit(1);
    }
}

/// Exactly MAX_FRAC frac digits on every compare — every instant emitted
/// by to_canonical carries the full run, frac-less forms included (a bare
/// "…45" is a strict prefix of "…45.000…" and would mis-order otherwise).
/// 19 covers real (≤ nanosecond) and pathological ≤19; longer fracs
/// truncate, so >19-digit fracs COLLAPSE: distinct instants compare equal
/// (extra keeps on --since, one final bucket lost on --until, line-vs-line
/// order blurs). Compare numerically via chrono if that ever matters.
const MAX_FRAC: usize = 19;
const ZEROS: &str = "0000000000000000000"; // MAX_FRAC zeros
const NINES: &str = "9999999999999999999"; // MAX_FRAC nines (max_instant)
// both pads are MAX_FRAC-wide by construction — checked at compile time so
// a MAX_FRAC change can't leave a stranded const behind
const _: () = assert!(ZEROS.len() == MAX_FRAC && NINES.len() == MAX_FRAC);

/// All bytes numeric — the one digit predicate (strip_tz's tz tails,
/// assert_canonical_shape's field slices).
fn all_digits(b: &[u8]) -> bool {
    b.iter().all(|c| c.is_ascii_digit())
}

/// tz-naive suffix strip (call AFTER space→'T'): trailing 'Z' or a
/// ±dd(:?dd)? tail. Byte checks, not regex — every line passes here.
/// The ':' check keeps the ± branch off plain instants (dates carry
/// '-' too, but never a ':' three bytes from the end).
fn strip_tz(out: &mut String) {
    let n = out.len();
    if out.ends_with('Z') {
        out.truncate(n - 1);
        return;
    }
    if n < 6 {
        return;
    }
    let b = out.as_bytes();
    let sign = |i: usize| b[i] == b'+' || b[i] == b'-';
    let six = n - 6;
    let five = n - 5;
    if sign(six) && all_digits(&b[six + 1..n - 3]) && b[n - 3] == b':' && all_digits(&b[n - 2..]) {
        out.truncate(six); // ±dd:dd
    } else if sign(five) && all_digits(&b[five + 1..]) {
        out.truncate(five); // ±dddd
    }
}

/// ISO 8601 end-of-day "T24:00(:00…0)": is_full_iso validates through it,
/// to_canonical rewrites exactly what it accepts — the rule's one home.
/// The '.'-check is insurance, not enforcement: frac-on-minutes is
/// rejected wholesale by FULL_ISO_RE (pointer lives there).
fn is_eod_24(b: &[u8]) -> bool {
    b[11] == b'2' && b[12] == b'4' && b[13] == b':'
        && b[14] == b'0' && b[15] == b'0'
        && b.get(16) != Some(&b'.')
        && (b.len() < 19 || b[16] != b':' || (b[17] == b'0' && b[18] == b'0'))
}

/// THE single canonical-for-compare writer: space→'T' (" " < "T" would
/// mis-order), the tz suffix strips whole (compare is tz-naive — a '+'
/// or 'Z' tail itself would mis-order equal instants), date-only = day
/// start, minute precision pads to ":00", and every instant ends with
/// MAX_FRAC frac digits. Lines and bounds both land here — one
/// invariant, no drift, asserted by assert_canonical_shape at BOTH exits
/// (any drift anywhere in the pipeline panics loud, never mis-compares).
fn to_canonical(s: &str, out: &mut String) {
    out.clear();
    if s.len() > 10 && s.as_bytes()[10] == b' ' {
        out.push_str(&s[..10]);
        out.push('T');
        out.push_str(&s[11..]);
    } else {
        out.push_str(s);
    }
    strip_tz(out);
    if out.len() == 10 {
        // date-only = day start (bounds: --until handles its own convention)
        out.push_str("T00:00:00");
        out.push('.');
        out.push_str(ZEROS);
        assert_canonical_shape(out);
        return;
    }
    // ISO 8601 end-of-day "T24:00(:00)" = the NEXT day's 00:00:00 — left
    // alone, "…T24:…" would sort after everything yet before the next
    // day's "T00:…" (compare order broken across the boundary).
    if out.len() >= 16 && is_eod_24(out.as_bytes()) {
        // Residual = tail after the "…T24:00(:00)" head: empty for the
        // minute-precision form ("T24:00", len 16). .get() instead of a
        // slice keeps that case panic-free without bounds bookkeeping —
        // FULL_ISO_RE is [0-9]-pinned above, so the tail is ascii and
        // all_digits below cannot panic (no multibyte net needed).
        let residual = out.get(19..).unwrap_or("").to_string();
        // Only empty or '.'-led digit tails are real frac grammar.
        // ponytail: drift net, second copy of FULL_ISO_RE's frac
        // grammar — the None clamp branch below DISCARDS the residual
        // silently, so this stays loud where the final gate is blind.
        // Keep in sync if the frac grammar ever widens (comma frac
        // etc.).
        assert!(
            residual.is_empty()
                || (residual.len() > 1
                    && residual.starts_with('.')
                    && all_digits(residual[1..].as_bytes())),
            "EOD 24:00 residual drift: {out:?}"
        );
        let next = next_day(&out[..10]);
        out.clear();
        match next {
            Some(d) => {
                out.push_str(&d);
                out.push_str("T00:00:00");
                out.push_str(&residual);
            }
            // 9999-12-31 end-of-day = the last instant: clamp, one shape
            None => max_instant(out),
        }
    }
    // is_full_iso guarantees at least HH:MM after 'T': partial iff byte 16
    // is not the ':' that would start seconds (nothing is left after the
    // tz strip, so a bare "HH:MM" tail is partial)
    if {
        let b = out.as_bytes();
        out.len() >= 16 && b[10] == b'T' && b[13] == b':' && b.get(16) != Some(&b':')
    } {
        // FULL_ISO_RE forbids frac-on-minutes (a frac may only follow
        // seconds). The end shape gate CANNOT catch this drift: the ":00"
        // insert below would normalize ".5 minutes" into an all-digit
        // canonical string ("half a second"), silently mis-ordering every
        // compare — so it panics HERE, before the pad.
        assert!(out.len() == 16, "frac-on-minutes drift: {out:?}");
        out.insert_str(16, ":00");
    }
    if let Some(i) = out.find('.') {
        let run = out.len() - i - 1;
        if run < MAX_FRAC {
            out.push_str(&ZEROS[..MAX_FRAC - run]);
        } else {
            out.truncate(i + 1 + MAX_FRAC);
        }
    } else {
        out.push('.');
        out.push_str(ZEROS);
    }
    assert_canonical_shape(out);
}

/// Normalize a matched timestamp for lexicographic compare: always
/// canonical — every instant takes the same shape, so no fast-path
/// predicate can drift from the writer. Writes into `buf` (reused
/// capacity; ~20 bytes memcpy, no per-line alloc).
fn normalize_iso<'a>(ts: &'a str, buf: &'a mut String) -> &'a str {
    to_canonical(ts, buf);
    buf.as_str()
}

/// Bounds canonicalize via to_canonical (space→T, padding, frac width).
/// --until is <, so a DATE-ONLY bound means NEXT-day start: "--until 2026-10-09"
/// keeps all of the 9th instead of excluding the whole end day. Padded
/// precisions keep their instant (exclusive at the pad instant).
/// ponytail: bounds normalized at this layer so callers can't forget the
/// convention; offset handling stays tz-naive — chrono if that ever matters.
fn canonical_bound(s: &str, until: bool) -> String {
    if until && s.len() == 10 {
        // RAW date-only: next-day start (padding as an exclusive bound
        // would exclude the whole end day). No next day exists past
        // 9999-12-31: clamp to the max instant (nines frac) — the whole
        // end day survives, and the string stays an embeddable literal.
        let mut out = String::new();
        match next_day(s) {
            Some(d) => to_canonical(&d, &mut out),
            None => max_instant(&mut out),
        }
        return out;
    }
    let mut out = String::new();
    to_canonical(s, &mut out);
    out
}

/// One date increment, no chrono: manual month/leap arithmetic. Returns
/// the DATE alone (callers write their own "T…" time) or None when there
/// is no next day (9999-12-31).
fn next_day(date: &str) -> Option<String> {
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
    if y > 9999 {
        return None; // 9999-12-31 has no next day (a "10000-…" string
                     // would sort BEFORE "9999-…")
    }
    Some(format!("{:04}-{:02}-{:02}", y, m, d))
}

/// Clamp target for bounds escaping 9999-12-31: the maximal canonical
/// instant. The frac is NINES, not zeros — an exclusive until at the last
/// day must admit instants inside the final second (.5 >= a zeros-pad
/// bound would drop them; pad ZEROS is to_canonical's shape by default).
/// ponytail: derived through to_canonical so the shape has one source; the
/// NINES const is compile-time-checked against MAX_FRAC.
fn max_instant(out: &mut String) {
    to_canonical("9999-12-31T23:59:59", out); // gate-checked: '.' at 19
    // ZEROS → NINES: digit-for-digit swap of the MAX_FRAC-width run
    // (width compile-time-pinned) over the to_canonical-gated shape —
    // shape survives by construction, so no runtime net is added here.
    out.replace_range(20.., NINES);
}

enum Run {
    /// survey: pattern frequencies to stderr, no stream output
    Stats,
    /// group by ID: full sessions or the compact --summary map
    Sessions,
    /// plain one-line-per-event stream
    Stream,
}

/// Trace/summary share collection and emission; the variant only changes what
/// a session keeps. BTreeMap = stable, reproducible order across runs.
enum Session {
    /// full trace: every event line of the session
    Full(Vec<String>),
    /// summary: event count, first event, last event — IDs always preserved
    Summary(usize, String, String),
}

struct RunResult {
    stats_map: Option<HashMap<String, usize>>,
    sessions: Option<BTreeMap<String, Session>>,
    stats_capped: bool,
    sessions_capped: bool,
    session_lines_capped: bool,
    dedup_inserts: usize,
}

/// Input source: byte lines (file/stdin/gcloud) or decoded bq CSV rows.
/// csv::Reader skips the header row and handles quoted fields with
/// embedded newlines/commas — raw --format=csv lines would corrupt them.
enum Source {
    Lines(Box<dyn BufRead>),
    Rows(csv::Reader<ChildStdout>),
}

/// Shared per-line pipeline: read bytes → query reject → time range → trim.
/// read_until (bytes) + lossy decode: one invalid UTF-8 byte degrades to
/// U+FFFD on that line instead of erroring and truncating the stream.
struct Pipeline {
    source: Source,
    re_timestamp: Regex,
    re_boilerplate: Regex,
    query: Option<String>,
    since_valid: Option<String>,
    until_valid: Option<String>,
    range_filter: bool,
    /// child-fetcher tool name ("gcloud"/"bq") — blank counts/notes gated on it
    child_tool: Option<&'static str>,
    ts_norm: String,
    warned_non_iso: bool,
    read_error: Option<io::Error>,
    early_cut: bool,
    empty_entries: usize,
    line_no: usize,
    raw_buf: Vec<u8>,
}

impl Pipeline {
    /// Next line that survives query + time range + trim, with its input
    /// line number (for Source::Rows: record number — the header row is
    /// skipped). None = end of input (on read error, read_error is set).
    fn next_trimmed(&mut self) -> Option<(usize, String)> {
        loop {
            let line_cow = match &mut self.source {
                Source::Lines(r) => {
                    self.raw_buf.clear();
                    match r.read_until(b'\n', &mut self.raw_buf) {
                        Ok(0) => return None,
                        Ok(_) => {}
                        Err(e) => {
                            self.read_error = Some(e);
                            return None;
                        }
                    }
                    self.line_no += 1;
                    String::from_utf8_lossy(&self.raw_buf)
                }
                Source::Rows(r) => match r.byte_records().next() {
                    Some(Ok(rec)) => {
                        self.line_no += 1;
                        // Columns join with ' ' (trim drops it): stripping the
                        // timestamp column leaves no separator residue, and a
                        // row that trims to nothing counts as an empty entry.
                        let mut line = String::new();
                        for (i, f) in rec.iter().enumerate() {
                            if i > 0 { line.push(' '); }
                            line.push_str(&String::from_utf8_lossy(f));
                        }
                        // flatten to one line: a quoted field's CRLF pair
                        // collapses to one space; a lone carriage return
                        // mid-line is content and stays verbatim (CR at row
                        // edges is trim whitespace, as in every source)
                        if line.contains(['\n', '\r']) {
                            line = line.replace("\r\n", " ");
                            line = line.replace('\n', " ");
                        }
                        std::borrow::Cow::Owned(line)
                    }
                    None => return None,
                    Some(Err(e)) => {
                        // count the failed record so the warning locates it
                        self.line_no += 1;
                        self.read_error = Some(e.into());
                        return None;
                    }
                },
            };
            let line = line_cow.trim_end_matches(['\n', '\r']);

            // Fast rejects FIRST, before any allocation
            if let Some(q) = &self.query {
                if !line.contains(q) { continue; }
            }

            // Time range: the first FULL-ISO match, wherever it sits
            // (brackets/level prefixes are common): an invalid earlier
            // match (bad calendar date) must not shield the line or
            // shadow a valid ts later in it. Apache-style lines have no
            // ISO match at all and bypass entirely.
            // ponytail: ts fields come first in real formats, so the
            // first full-ISO match is the intended ts; ISO strings sort
            // chronologically and space separators normalize to 'T'.
            if self.range_filter {
                let lead = self.re_timestamp.find_iter(line)
                    .find(|m| is_full_iso(m.as_str()))
                    .map(|m| m.as_str());
                if let Some(ts) = lead {
                    let ts = normalize_iso(ts, &mut self.ts_norm);
                    if let Some(s) = &self.since_valid {
                        if ts < s.as_str() { continue; }
                    }
                    if let Some(u) = &self.until_valid {
                        if ts >= u.as_str() { continue; }
                    }
                } else if !self.warned_non_iso && self.re_timestamp.is_match(line) {
                    self.warned_non_iso = true;
                    eprintln!("Warning: --since/--until needs a leading ISO (YYYY-...) timestamp; lines with non-ISO leading timestamps bypass the range filter.");
                }
            }

            let trimmed = self.re_timestamp.replace_all(line, "");
            let trimmed = self.re_boilerplate.replace_all(&trimmed, "");
            let final_line = trimmed.trim();
            if final_line.is_empty() {
                // A fetcher row/entry that contributes nothing: count it so
                // vanished records are visible (gcloud blank entries, bq rows
                // whose text column is empty). Counted late so --query rows
                // the user didn't ask for are never tallied as losses.
                if self.child_tool.is_some() { self.empty_entries += 1; }
                continue;
            }
            return Some((self.line_no, final_line.to_string()));
        }
    }
}

/// Spawn a child fetcher (gcloud/bq): piped stdout, inherited stderr.
/// Returns (raw stdout pipe, child we own for wait/kill).
fn spawn_fetcher(tool: &'static str, argv: Vec<String>) -> (ChildStdout, Child) {
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

/// Canonical-shape gate: to_canonical's ONLY legal output — full datetime,
/// len 39 (19 + '.' + MAX_FRAC), separators pinned, every other byte a
/// digit. ONE gate at every to_canonical exit covers every compare (bounds
/// and per-line alike) and every pushdown embedder; drift panics loud,
/// naming the string.
fn assert_canonical_shape(canonical: &str) {
    let b = canonical.as_bytes();
    // each byte of 0..39 covered exactly once: field slices via the one
    // all_digits predicate, separators pinned
    let ok = b.len() == 19 + 1 + MAX_FRAC
        && all_digits(&b[0..4]) && b[4] == b'-'
        && all_digits(&b[5..7]) && b[7] == b'-'
        && all_digits(&b[8..10]) && b[10] == b'T'
        && all_digits(&b[11..13]) && b[13] == b':'
        && all_digits(&b[14..16]) && b[16] == b':'
        && all_digits(&b[17..19]) && b[19] == b'.'
        && all_digits(&b[20..]);
    assert!(ok, "canonical shape drift: {canonical:?}");
}

/// Server-pushdown literal for an --until bound: `<` one second past the
/// bound's floor second. (`<=` the floor second would EXCLUDE (floor, frac)
/// entries the local filter keeps — a plain "…45" literal is instant 45.0,
/// not the whole second.) None only when no next second exists (9999) —
/// the caller drops the clause; the local exact filter still bounds it.
fn until_server_bound(canonical: &str) -> Option<String> {
    assert_canonical_shape(canonical);
    // ponytail: plain String — called once per run (bounds canonicalize
    // once), a Cow's borrowed/owned split costs more than the 10-byte
    // alloc it saves; only the rare h==24 rollover needs an owned date
    let mut date = canonical[..10].to_string();
    let mut h: u32 = canonical[11..13].parse().expect("canonical hour");
    let mut m: u32 = canonical[14..16].parse().expect("canonical minute");
    let mut s: u32 = canonical[17..19].parse().expect("canonical second");
    s += 1;
    if s == 60 { s = 0; m += 1; }
    if m == 60 { m = 0; h += 1; }
    if h == 24 {
        h = 0;
        date = next_day(&date)?;
    }
    Some(format!("{date}T{h:02}:{m:02}:{s:02}"))
}

/// since-pushdown literal: the bound's floor second — entries from it on
/// are fetched, sub-second extras before the exact local filter's cut are
/// dropped locally.
/// ponytail: second-precision pushdown, no frac at all — GCP parser risk
/// gone; finer narrowing if the fetch ever needs it.
fn server_ts(canonical: &str) -> &str {
    assert_canonical_shape(canonical);
    &canonical[..19]
}

/// Spawn gcloud / bq / open file / stdin. Returns the input source and any
/// child fetcher we own. gcloud mode pushes canonical time bounds into the
/// SERVER-side filter (valid ISO by is_full_iso) to avoid downloading months
/// of entries that local filtering would drop.
/// ponytail: bq mode can't push bounds — the SQL is the user's — so
/// --since/--until filter locally; date it in the query itself.
fn make_reader(
    args: &Args,
    since_valid: &Option<String>,
    until_valid: &Option<String>,
) -> (Source, Option<(&'static str, Child)>) {
    if let Some(filter) = &args.gcloud {
        let mut gc_filter = filter.clone();
        // bounds already canonicalized: no space separator remains. Pushdown
        // is second-precision (server_ts / until_server_bound): since >= its
        // floor second, until < its next second — the local exact filter
        // trims whatever the wider fetch returns. Comments claim nothing
        // about RFC 3339: these are plain gcloud timestamp literals (no
        // offset), the shape this pushdown has carried since it existed.
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
        // ponytail: no --limit pushdown for --max-lines; the early-cut kill
        // already bounds the download.
        let (stdout, child) = spawn_fetcher("gcloud", gc_args);
        (Source::Lines(Box::new(BufReader::new(stdout))), Some(("gcloud", child)))
    } else if let Some(sql) = &args.bq {
        // --max_rows pushed at 10M because bq silently caps selects at
        // 1000 rows; csv::Reader decodes rows (quotes, embedded newlines).
        let bq_args = vec![
            "query".to_string(),
            "--use_legacy_sql=false".to_string(),
            "--format=csv".to_string(),
            "--max_rows=10000000".to_string(),
            sql.clone(),
        ];
        let (stdout, child) = spawn_fetcher("bq", bq_args);
        // ponytail: csv::Reader buffers internally — no BufReader here.
        // flexible(true) tolerates a ragged record instead of aborting.
        let rows = csv::ReaderBuilder::new().flexible(true).from_reader(stdout);
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

/// Stream mode: dedup + emit as we go; returns dedup insertions.
fn run_stream(
    proc: &mut Pipeline,
    mut bloom: Option<&mut Bloom<String>>,
    args: &Args,
    outbuf: &mut Sink,
) -> usize {
    let mut count = 0usize;
    let mut dedup_inserts = 0usize;
    while let Some((line_no, line)) = proc.next_trimmed() {
        if let Some(max) = args.max_lines {
            if count >= max {
                proc.early_cut = true;
                break;
            }
        }
        if let Some(ref mut b) = bloom {
            // single probe: check + insert in one hash pass
            if b.check_and_set(&line) { continue; }
            // Track actual insertions: saturating past capacity = silent data loss
            dedup_inserts += 1;
        }
        if args.json {
            // plain usize/String struct — serialization cannot fail
            let json = serde_json::to_string(&LogEntry {
                index: line_no,
                content: line,
            }).unwrap();
            emit(outbuf, &json);
        } else {
            emit(outbuf, &line);
        }
        count += 1;
    }
    dedup_inserts
}

/// Stats mode: survey the whole file; returns counts + whether capped.
fn run_stats(proc: &mut Pipeline) -> (HashMap<String, usize>, bool) {
    let mut stats_map: HashMap<String, usize> = HashMap::new();
    let mut capped = false;
    while let Some((_, line)) = proc.next_trimmed() {
        // get_mut fast path — no String alloc for a line already seen.
        // Cap gates only new insertions; existing counters keep advancing.
        if let Some(c) = stats_map.get_mut(&line) {
            *c += 1;
        } else if stats_map.len() < MAX_STATS_PATTERNS {
            stats_map.insert(line, 1);
        } else {
            capped = true;
        }
    }
    (stats_map, capped)
}

/// Sessions mode: collect Full or Summary per ID. Caps gate NEW sessions /
/// NEW lines only — existing sessions keep collecting past their caps.
fn collect_sessions(
    proc: &mut Pipeline,
    args: &Args,
    re: &Regex,
) -> (BTreeMap<String, Session>, bool, bool) {
    let summary = args.summary;
    let mut sessions: BTreeMap<String, Session> = BTreeMap::new();
    let mut count = 0usize;
    let mut sessions_capped = false;
    let mut lines_capped = false;
    while let Some((_, line)) = proc.next_trimmed() {
        if let Some(max) = args.max_lines {
            if count >= max {
                proc.early_cut = true;
                break;
            }
        }
        let Some(caps) = re.captures(&line) else { continue; };
        // group 0 is always present when captures() succeeds
        let id = caps.get(1)
            .unwrap_or_else(|| caps.get(0).unwrap())
            .as_str().to_string();
        // len-before-entry: entry() holds the map borrow, can't re-len() here
        let at_cap = sessions.len() >= MAX_TRACE_SESSIONS;
        match sessions.entry(id) {
            btree_map::Entry::Occupied(mut e) => match e.get_mut() {
                Session::Full(v) => {
                    if v.len() < MAX_LINES_PER_SESSION {
                        v.push(line);
                        count += 1;
                    } else {
                        lines_capped = true;
                    }
                }
                Session::Summary(n, _, last) => {
                    // auto-ref-mut binding; reuse the buffer, no per-event alloc
                    *n += 1;
                    last.clear();
                    last.push_str(&line);
                    count += 1;
                }
            },
            btree_map::Entry::Vacant(e) => {
                if at_cap {
                    sessions_capped = true;
                } else {
                    let l = line.to_string();
                    e.insert(if summary {
                        Session::Summary(1, l.clone(), l)
                    } else {
                        Session::Full(vec![l])
                    });
                    count += 1;
                }
            }
        }
    }
    (sessions, sessions_capped, lines_capped)
}

/// Emit collected sessions — one JSON object or one text block per ID.
fn emit_sessions(sessions: &BTreeMap<String, Session>, json: bool, outbuf: &mut Sink) {
    if json {
        for (id, s) in sessions {
            let value = match s {
                Session::Full(lines) => serde_json::json!({"id": id, "events": lines}),
                Session::Summary(count, first, last) => serde_json::json!({
                    "id": id, "count": count, "first": first, "last": last
                }),
            };
            let out = serde_json::to_string(&value).unwrap();
            emit(outbuf, &out);
        }
    } else {
        let summary = sessions.values().next()
            .map_or(false, |s| matches!(s, Session::Summary(..)));
        if summary {
            emit(outbuf, &format!("\n=== TRACE SUMMARY: {} sessions ===", sessions.len()));
        }
        for (id, s) in sessions {
            match s {
                Session::Full(lines) => {
                    emit(outbuf, &format!("\n--- [{}] ---\n{}", id, lines.join("\n")));
                }
                Session::Summary(count, first, last) => {
                    // 1-event session: first == last — print once, singular label
                    let body = if *count == 1 {
                        first.to_string()
                    } else {
                        format!("{}\n{}", first, last)
                    };
                    emit(outbuf, &format!(
                        "--- [{}] {} event{} ---\n{}", id, count,
                        if *count == 1 { "" } else { "s" }, body
                    ));
                }
            }
        }
    }
}

fn main() {
    let args = Args::parse();

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
        if let Some(b) = bound {
            if !is_full_iso(b) {
                eprintln!(
                    "Error: --{} {:?} is not a full ISO date (YYYY-MM-DD or with time).",
                    label, b
                );
                exit(2);
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

    // Compile ALL regexes BEFORE spawning gcloud: a bad pattern must not
    // orphan a running child process.
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
    let mut child = fetched_child.map(|(_, c)| c);

    let mut proc = Pipeline {
        source: reader,
        re_timestamp,
        re_boilerplate,
        query: args.query.clone(),
        range_filter,
        child_tool,
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
    let mut bloom = if matches!(run, Run::Stream) && args.dedup {
        Some(Bloom::new_for_fp_rate(10_000_000, 0.01).expect("Failed to init bloom filter"))
    } else {
        None
    };

    let mut out = io::stdout().lock();
    let mut outbuf: Sink = BufWriter::new(&mut out);

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
            }
        }
        Run::Sessions => {
            let (map, sessions_capped, session_lines_capped) =
                collect_sessions(&mut proc, &args, trace_re.as_ref().unwrap());
            RunResult {
                stats_map: None,
                sessions: Some(map),
                stats_capped: false,
                sessions_capped,
                session_lines_capped,
                dedup_inserts: 0,
            }
        }
        Run::Stream => {
            let dedup_inserts = run_stream(&mut proc, bloom.as_mut(), &args, &mut outbuf);
            RunResult {
                stats_map: None,
                sessions: None,
                stats_capped: false,
                sessions_capped: false,
                session_lines_capped: false,
                dedup_inserts,
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

    if result.dedup_inserts > 10_000_000 {
        eprintln!("Warning: --dedup exceeded 10M unique lines; bloom filter is saturated and unique lines may be dropped as false duplicates.");
    }
    if let Some(tool) = child_tool {
        if empty_entries > 0 {
            eprintln!("Note: {} empty entr{} from {} were skipped.", empty_entries, if empty_entries == 1 { "y" } else { "ies" }, tool);
        }
    }
    if result.stats_capped {
        eprintln!("Warning: --stats reached its {} unique-pattern cap; later patterns untracked (existing counts are complete).", MAX_STATS_PATTERNS);
    }
    if result.sessions_capped {
        eprintln!("Warning: trace session cap ({}) reached; later sessions not collected.", MAX_TRACE_SESSIONS);
    }
    if result.session_lines_capped {
        eprintln!("Warning: some sessions hit the {}-line cap; their tails were cut.", MAX_LINES_PER_SESSION);
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
        eprintln!("\n=== LOG PATTERN STATISTICS ===");
        for (p, c) in &sorted {
            eprintln!("{:>7} | {}", c, p);
        }
        eprintln!("==============================");
    }

    if let Some(sessions) = result.sessions {
        emit_sessions(&sessions, args.json, &mut outbuf);
    }
    flush(&mut outbuf);

    // We cut the stream early (--max-lines): kill gcloud outright. Rust
    // ignores SIGPIPE and children inherit it, so gcloud would die on EPIPE
    // with a nonzero exit — reported as a failure we caused.
    if early_cut {
        if let Some(child) = child.as_mut() { let _ = child.kill(); }
    }
    if let Some(child) = child.as_mut() {
        match child.wait() {
            Ok(status) if !status.success() => {
                if early_cut {
                    eprintln!("Note: {} stream cut early (--max-lines).", child_tool.unwrap_or("fetcher"));
                } else {
                    // Propagate: scripts piping cardio must see fetch failures
                    eprintln!("Error: {} exited with {}", child_tool.unwrap_or("fetcher"), status);
                    exit(1);
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("Warning: failed to wait for {}: {}", child_tool.unwrap_or("fetcher"), e),
        }
    }

    if read_error.is_some() {
        // Truncated input is a failure even though we emitted partial results
        exit(1);
    }
}