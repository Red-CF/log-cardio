//! Trace grouping: collect per-ID sessions and emit them. Full mode keeps
//! every event line; summary keeps count + first/last. Caps gate NEW
//! sessions / NEW lines only — existing sessions keep collecting past
//! their caps.

use crate::pipeline::{Pipeline, emit, Sink};
use crate::Args;
use regex::Regex;
use std::collections::{btree_map, BTreeMap, HashMap};

pub(crate) const MAX_TRACE_SESSIONS: usize = 10_000;
pub(crate) const MAX_LINES_PER_SESSION: usize = 1_000;

/// What a run does with the pipeline output.
pub(crate) enum Run {
    /// survey: pattern frequencies to stderr, no stream output
    Stats,
    /// group by ID: full sessions or the compact --summary map
    Sessions,
    /// plain one-line-per-event stream
    Stream,
}

pub(crate) struct RunResult {
    pub(crate) stats_map: Option<HashMap<String, usize>>,
    pub(crate) sessions: Option<BTreeMap<String, Session>>,
    pub(crate) stats_capped: bool,
    pub(crate) sessions_capped: bool,
    pub(crate) session_lines_capped: bool,
    pub(crate) dedup_inserts: usize,
    pub(crate) dropped_dups: usize,
    // Stream mode: the --tail/lean window overflowed (input had more kept
    // lines than the window) — feeds the "kept the last N" stderr note
    pub(crate) window_full: bool,
}

/// Trace/summary share collection and emission; the variant only changes what
/// a session keeps. BTreeMap = stable, reproducible order across runs.
pub(crate) enum Session {
    /// full trace: every event line of the session
    Full(Vec<String>),
    /// summary: event count, first event, last event — IDs always preserved
    Summary(usize, String, String),
}

/// Sessions mode: collect Full or Summary per ID. Caps gate NEW sessions /
/// NEW lines only — existing sessions keep collecting past their caps.
pub(crate) fn collect_sessions(
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
        if let Some(max) = args.max_lines
            && count >= max {
                proc.early_cut = true;
                break;
            }
        // capture on the UNCLAMPED line, clamp only what we store: a trace
        // id sitting further into a huge line must still group
        let Some(caps) = re.captures(&line) else { continue; };
        // group 0 is always present when captures() succeeds
        let id = caps.get(1)
            .unwrap_or_else(|| caps.get(0).unwrap())
            .as_str().to_string();
        let line = proc.clamp_fields(&line);
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
/// `--top N` keeps only the biggest sessions (by event count) and returns
/// how many were dropped for the honest note: with hundreds of IDs the
/// compact map itself was still ~80 tokens per ID, unread (real-agent
/// feedback: 144 sessions = 11.6k tokens, none read).
pub(crate) fn emit_sessions(
    sessions: &BTreeMap<String, Session>,
    top: Option<usize>,
    json: bool,
    outbuf: &mut Sink,
) -> usize {
    // size = events kept per session (summary count / full lines)
    let size = |s: &Session| match s {
        Session::Full(v) => v.len(),
        Session::Summary(n, ..) => *n,
    };
    let mut view: Vec<(&String, &Session)> = sessions.iter().collect();
    view.sort_by_key(|(_id, s)| std::cmp::Reverse(size(s)));
    let dropped = top.map_or(0, |n| view.len().saturating_sub(n));
    if dropped > 0 {
        // ONE channel per mode: text gets the header on stdout here, json
        // gets nothing on stdout (main's stderr note is the json witness)
        let n = top.unwrap_or(0);
        if !json {
            emit(outbuf, &format!(
                "\n=== showing the top {} of {} sessions ({} smaller dropped; --top N adjusts) ===",
                n, view.len() + dropped, dropped
            ));
        }
        view.truncate(n);
    }
    if !json {
        let summary = view.iter().any(|(_, s)| matches!(s, Session::Summary(..)));
        if summary {
            emit(outbuf, &format!("\n=== TRACE SUMMARY: {} sessions ===", view.len()));
        }
    }
    for (id, s) in &view {
        emit_session(id, s, json, outbuf);
    }
    dropped
}

fn emit_session(id: &str, s: &Session, json: bool, outbuf: &mut Sink) {
    if json {
        let value = match s {
            Session::Full(lines) => serde_json::json!({"id": id, "events": lines}),
            Session::Summary(count, first, last) => serde_json::json!({
                "id": id, "count": count, "first": first, "last": last
            }),
        };
        let out = serde_json::to_string(&value).unwrap();
        emit(outbuf, &out);
    } else {
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