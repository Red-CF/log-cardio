//! The --stats survey: counting (with the digit-run fold) and the two
//! output forms (stderr table, NDJSON). Mode wiring stays in main.

use crate::consts::MAX_STATS_PATTERNS;
use crate::pipeline::DIGIT_RUN;
use crate::pipeline::{Pipeline, Sink, emit};
use std::collections::HashMap;



/// Stats mode: survey the whole file; returns counts + whether capped.
pub(crate) fn run_stats(proc: &mut Pipeline) -> (HashMap<String, usize>, bool) {
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

/// Census mode: count KEPT rows per selected column value (rows are the
/// verbatim tab-joined bq fields). Same caps and tie order as the survey:
/// it IS the survey over one fold, so both forms print the same way and
/// the agent needs no new mental model.
pub(crate) fn run_group(
    proc: &mut Pipeline,
    cols: &[usize],
) -> (HashMap<String, usize>, bool) {
    let mut stats_map: HashMap<String, usize> = HashMap::new();
    let mut capped = false;
    while let Some((_, line)) = proc.next_trimmed() {
        let key: String = if cols.len() == 1 {
            match line.split('\t').nth(cols[0]) {
                Some(v) => v.to_string(),
                None => continue,
            }
        } else {
            let fields: Vec<&str> = line.split('\t').collect();
            let mut parts = Vec::new();
            for &c in cols {
                match fields.get(c) {
                    Some(v) => parts.push(v.to_string()),
                    None => { parts.clear(); break; }
                }
            }
            if parts.is_empty() { continue; }
            parts.join("|")
        };
        if let Some(c) = stats_map.get_mut(&key) {
            *c += 1;
        } else if stats_map.len() < MAX_STATS_PATTERNS {
            stats_map.insert(key, 1);
        } else {
            capped = true;
        }
    }
    (stats_map, capped)
}

/// Same top-20 + deterministic count-desc/pattern-asc tie order for both
/// forms. ponytail: O(n) top-20 partition, full sort only on the 20.
pub(crate) fn sorted_top20(map: HashMap<String, usize>) -> Vec<(String, usize)> {
    // by value: the map dies right after the top-20 — no double clones of
    // up to 100k pattern strings
    let mut sorted: Vec<_> = map.into_iter().collect();
    let cmp = |a: &(String, usize), b: &(String, usize)| b.1.cmp(&a.1).then(a.0.cmp(&b.0));
    if sorted.len() > 20 {
        sorted.select_nth_unstable_by(19, cmp);
    }
    sorted.truncate(20);
    sorted.sort_by(cmp);
    sorted
}

/// stderr table (the human/agent default).
pub(crate) fn print_table(map: HashMap<String, usize>) {
    let sorted = sorted_top20(map);
    eprintln!("\n=== LOG PATTERN STATISTICS ===");
    for (p, c) in &sorted {
        eprintln!("{:>7} | {}", c, p);
    }
    eprintln!("==============================");
}

/// NDJSON on stdout (the machine channel).
pub(crate) fn print_json(map: HashMap<String, usize>, outbuf: &mut Sink) {
    for (p, c) in sorted_top20(map) {
        emit(outbuf, &serde_json::json!({"pattern": p, "count": c}).to_string());
    }
}