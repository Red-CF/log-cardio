//! The stderr witnesses: everything the run GAVE UP must be said, never
//! silently (the read-stays-honest contract). One struct fills in main,
//! one emit writes them all — adding a witness starts here, not in the
//! orchestration.

use crate::consts::{LEAN_MAX_LINES, MAX_STATS_PATTERNS};
use crate::sessions;
use crate::sessions::RunResult;

/// Run facts the notes need; filled in main after the runners return.
pub(crate) struct Notes {
    pub explicit_tail: bool,
    pub explicit_head: bool,
    /// the effective head cap, for the early-cut note
    pub head_size: usize,
    pub child_tool: Option<&'static str>,
    pub early_cut: bool,
    pub stopped_at_line: usize,
    pub empty_entries: usize,
}

/// Everything dropped, capped, cut or stopped — with the counts.
pub(crate) fn emit(
    n: &Notes,
    result: &RunResult,
    sessions_top_dropped: usize,
    sessions_json: bool,
    sessions_top: usize,
) {
    // Honest stderr signals: the default pipeline drops and caps, so the
    // agent (or the person reading the log) must SEE exactly what was
    // given up — never silently.
    if result.dropped_dups > 0 {
        eprintln!(
            "Note: dedup dropped {} repeat line{}. Under the 5000-unique-lines mark dedup is exact; above it, a few may actually be unique (--raw keeps everything).",
            result.dropped_dups,
            if result.dropped_dups == 1 { "" } else { "s" }
        );
    }
    if result.window_full && !n.explicit_tail {
        eprintln!(
            "Note: kept the last {} lines — the most recent; the rest of the input was read and trimmed (--raw removes the cap; --query TEXT narrows before reading).",
            LEAN_MAX_LINES
        );
    }
    if n.early_cut {
        eprintln!(
            "Note: output stopped at the head cap ({} lines); the rest of the input was not read.{}",
            n.head_size,
            if n.explicit_head { "" } else { " (the lean default; --raw removes it)" }
        );
    }
    if result.dedup_inserts > 10_000_000 {
        eprintln!("Warning: --dedup exceeded 10M unique lines; bloom filter is saturated and unique lines may be dropped as false duplicates.");
    }
    if let Some(tool) = n.child_tool
        && n.empty_entries > 0 {
            eprintln!("Note: {} empty entr{} from {} were skipped.", n.empty_entries, if n.empty_entries == 1 { "y" } else { "ies" }, tool);
        }
    // Real-agent feedback: an empty result looked like a failure because
    // nothing said otherwise. EVERY fetcher run states its volume — zero
    // included — so 0 rows has a witness; the job id is already in stderr
    // from the child.
    if let Some(tool) = n.child_tool {
        eprintln!("Note: {} streamed {} line{}.", tool, n.stopped_at_line, if n.stopped_at_line == 1 { "" } else { "s" });
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
    // text mode already carries the count inside its output header; the
    // json stream must stay clean — this note is its witness only
    if sessions_json && sessions_top_dropped > 0 {
        eprintln!(
            "Note: {} smaller session{} dropped (--top {}).",
            sessions_top_dropped, if sessions_top_dropped == 1 { "" } else { "s" }, sessions_top
        );
    }
}