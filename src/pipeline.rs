//! The per-line pipeline: read → query reject → time range → trim.
//! Shared by every run mode (stream/stats/sessions are callers); hosts
//! the stdout-sink helpers and the Stream runner.

use crate::timestamps::{is_full_iso, normalize_iso};
use crate::Args;
use bloomfilter::Bloom;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, Write};

/// Broken pipe (consumer left: head, grep -q) exits quietly; any other write
/// failure (ENOSPC etc.) is fatal — callers exit 1 so truncation is noticed.
pub(crate) fn on_write_err(e: io::Error, what: &str) {
    if e.kind() == io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("Error {}: {}", what, e);
}

pub(crate) fn emit<W: Write>(w: &mut W, text: &str) {
    if let Err(e) = writeln!(w, "{}", text) {
        on_write_err(e, "writing output");
        std::process::exit(1);
    }
}

/// Output sink shared by the mode runners.
pub(crate) type Sink<'w> = io::BufWriter<&'w mut io::StdoutLock<'static>>;

pub(crate) fn flush(outbuf: &mut Sink) {
    if let Err(e) = outbuf.flush() {
        on_write_err(e, "flushing output");
        std::process::exit(1);
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LogEntry {
    index: usize,
    content: String,
}

/// Shared per-line pipeline: read bytes → query reject → time range → trim.
/// read_until (bytes) + lossy decode: one invalid UTF-8 byte degrades to
/// U+FFFD on that line instead of erroring and truncating the stream.
pub(crate) struct Pipeline {
    pub(crate) source: crate::source::Source,
    pub(crate) re_timestamp: Regex,
    /// combined ts|boilerplate strip regex — the trim workhorse (one pass)
    pub(crate) re_trim: Regex,
    pub(crate) query: Option<String>,
    pub(crate) since_valid: Option<String>,
    pub(crate) until_valid: Option<String>,
    pub(crate) range_filter: bool,
    /// child-fetcher tool name ("gcloud"/"bq") — blank counts/notes gated on it
    pub(crate) child_tool: Option<&'static str>,
    /// --raw: verbatim fidelity — no trim of timestamps or boilerplate
    /// (real-agent feedback: the strip ate the bq timestamp column, losing
    /// temporal order; raw must hand over exactly what the source gave)
    pub(crate) verbatim: bool,
    /// --max-field-len: each tab-delimited field (whole line when there
    /// are no tabs) cuts to this many chars with a "…" marker — huge
    /// XML/bodies don't need SQL SUBSTR hacks
    pub(crate) max_field_len: Option<usize>,
    pub(crate) ts_norm: String,
    pub(crate) warned_non_iso: bool,
    pub(crate) read_error: Option<io::Error>,
    pub(crate) early_cut: bool,
    pub(crate) empty_entries: usize,
    pub(crate) line_no: usize,
    pub(crate) raw_buf: Vec<u8>,
}

impl Pipeline {
    /// Next line that survives query + time range + trim, with its input
    /// line number (for Source::Rows: record number — the header row is
    /// skipped). None = end of input (on read error, read_error is set).
    pub(crate) fn next_trimmed(&mut self) -> Option<(usize, String)> {
        loop {
            let line_cow = match &mut self.source {
                crate::source::Source::Lines(r) => {
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
                crate::source::Source::Rows(r) => match r.byte_records().next() {
                    Some(Ok(rec)) => {
                        self.line_no += 1;
                        // Columns join with '\t': field boundaries stay
                        // visible and an empty/null column leaves its
                        // separator (agents correlated trace columns; the
                        // old space-join made column N unattributable).
                        // Trim eats only END separators, so a stripped
                        // timestamp column leaves at most a trimmed edge.
                        let mut line = String::new();
                        for (i, f) in rec.iter().enumerate() {
                            if i > 0 { line.push('\t'); }
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
            if let Some(q) = &self.query
                && !line.contains(q) { continue; }

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
                    if let Some(s) = &self.since_valid
                        && ts < s.as_str() { continue; }
                    if let Some(u) = &self.until_valid
                        && ts >= u.as_str() { continue; }
                } else if !self.warned_non_iso && self.re_timestamp.is_match(line) {
                    self.warned_non_iso = true;
                    eprintln!("Warning: --since/--until needs a leading ISO (YYYY-...) timestamp; lines with non-ISO leading timestamps bypass the range filter.");
                }
            }

            if self.verbatim {
                return Some((self.line_no, self.clamp_fields(line)));
            }

            // bq CLI progress chatter ("Waiting on bqjob_r… (…)") can reach
            // the output stream depending on CLI version/piping; it is not
            // log content and never carries a timestamp. One cheap prefix
            // check, child-scoped to bq only.
            if self.child_tool == Some("bq") && line.starts_with("Waiting on bqjob") {
                continue;
            }

            // one combined pass: ts + boilerplate as one alternation, both
            // replaced with "" — identical result to the old two-pass trim
            // at half the regex cost (the visible lean/raw gap was ~0.6s
            // per M lines; bench.sh tracks this number)
            let trimmed = self.re_trim.replace_all(line, "");
            let final_line = trimmed.trim();
            if final_line.is_empty() {
                // A fetcher row/entry that contributes nothing: count it so
                // vanished records are visible (gcloud blank entries, bq rows
                // whose text column is empty). Counted late so --query rows
                // the user didn't ask for are never tallied as losses.
                if self.child_tool.is_some() { self.empty_entries += 1; }
                continue;
            }
            return Some((self.line_no, self.clamp_fields(final_line)));
        }
    }

    /// Field cap, applied only when the line is over budget (no alloc on
    /// the common path). Tab-delimited segments cut individually;
    /// tab-less lines count as one field. "…" marks the cut.
    fn clamp_fields(&self, line: &str) -> String {
        let Some(n) = self.max_field_len else {
            return line.to_string();
        };
        if line.contains('\t') {
            line.split('\t')
                .map(|f| clamp_one(f, n))
                .collect::<Vec<_>>()
                .join("\t")
        } else {
            clamp_one(line, n)
        }
    }
}

/// One field to at most `n` chars ("…" appended when cut). ≤n costs one
/// alloc only for the copy — acceptable at the return site (the trimmed
/// path already allocates).
fn clamp_one(field: &str, n: usize) -> String {
    if field.chars().count() <= n {
        return field.to_string();
    }
    let mut out: String = field.chars().take(n).collect();
    out.push('…');
    out
}

/// Stream mode: dedup + emit as we go; returns (dedup insertions,
/// dropped-suspect-duplicate lines, window-overflow) — the honest stderr
/// signals after the run.
pub(crate) fn run_stream(
    proc: &mut Pipeline,
    mut bloom: Option<&mut Bloom<String>>,
    args: &Args,
    outbuf: &mut Sink,
) -> (usize, usize, bool) {
    let mut dedup_inserts = 0usize;
    let mut dropped_dups = 0usize;
    // --tail or the lean/--max-lines cap: one window of the last N KEPT
    // lines, emitted after the loop. Recent beats old (agents read "what is
    // happening now", so the cap sacrifices the head of the range), and the
    // whole input is read — a cap no longer ends a stream early.
    let window_n = args.tail.or(args.max_lines);
    let mut tail_buf: VecDeque<(usize, String)> =
        window_n.map(VecDeque::with_capacity).unwrap_or_default();
    let mut window_full = false;
    while let Some((line_no, line)) = proc.next_trimmed() {
        if let Some(ref mut b) = bloom {
            // single probe: check + insert in one hash pass
            if b.check_and_set(&line) {
                dropped_dups += 1;
                continue;
            }
            // Track actual insertions: saturating past capacity = silent data loss
            dedup_inserts += 1;
        }
        if let Some(n) = window_n {
            // line_no preserved so --tail --json keeps input indices
            if tail_buf.len() >= n {
                tail_buf.pop_front();
                window_full = true;
            }
            tail_buf.push_back((line_no, line));
            continue;
        }
        if args.json {
            // plain usize/String struct — serialization cannot fail
            let json = serde_json::to_string(&LogEntry { index: line_no, content: line }).unwrap();
            emit(outbuf, &json);
        } else {
            emit(outbuf, &line);
        }
    }
    // tail/cap window: emit the buffered window (json kept in input-line
    // order)
    for (line_no, line) in tail_buf {
        if args.json {
            let json = serde_json::to_string(&LogEntry { index: line_no, content: line }).unwrap();
            emit(outbuf, &json);
        } else {
            emit(outbuf, &line);
        }
    }
    (dedup_inserts, dropped_dups, window_full)
}