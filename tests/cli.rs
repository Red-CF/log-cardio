#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// unique per test run: pid + counter — nanos collide when tests run in
/// parallel and one fixture overwrites another's file mid-test
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn cardio(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(args)
        .output()
        .expect("cardio binary should run");
    // a regression that exits early shows as empty output otherwise;
    // asserting status keeps the failure honest
    assert!(
        out.status.success(),
        "cardio exited {}: stderr: {:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Temp fixture removed on drop — even when an assert panics mid-test
struct Fixture(std::path::PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) { std::fs::remove_file(&self.0).ok(); }
}

fn fixture(content: &str) -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "cardio_test_{}_{}.log",
        std::process::id(),
        FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, content).expect("write fixture");
    Fixture(path)
}

#[test]
fn trims_timestamp_and_level() {
    let path = fixture("2026-10-09T12:00:00Z INFO User login ok\n2026-10-09T12:00:01Z DEBUG INFORMATION retained\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref()]);

    assert!(out.contains("User login ok"), "got: {out:?}");
    assert!(out.contains("INFORMATION retained"), "word INFO inside INFORMATION must survive; got: {out:?}");
}

#[test]
fn empty_after_trim_is_skipped() {
    let path = fixture("2026-10-09T12:00:00Z INFO\n2026-10-09T12:00:01Z DEBUG real event\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref()]);

    assert!(!out.contains("\n2026-"), "pure-noise lines must vanish");
    assert!(out.contains("real event"));
}

#[test]
fn since_excludes_earlier_days() {
    let path = fixture(
        "2026-10-08T23:00:00Z INFO day8\n2026-10-09T12:00:00Z INFO day9\n2025-01-01T00:00:00Z INFO old\n",
    );
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--since", "2026-10-09"]);

    assert!(out.contains("day9"));
    assert!(!out.contains("day8"));
    assert!(!out.contains("old"));
}

#[test]
fn groups_sessions_by_trace_id() {
    let path = fixture(
        "2026-10-09T12:00:00Z INFO req_42 start\n2026-10-09T12:00:01Z INFO req_42 fail\n2026-10-09T12:00:02Z INFO req_43 start\n",
    );
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--trace", "req_([0-9]+)"]);

    assert!(out.contains("--- [42] ---"));
    assert!(out.contains("req_42 start") && out.contains("req_42 fail"));
    assert!(out.contains("--- [43] ---"));
}

#[test]
fn json_output_is_lines_of_json() {
    let path = fixture("2026-10-09T12:00:00Z INFO hello\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--json"]);

    for line in out.lines().filter(|l| !l.is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("each line must be JSON: {e} in {line:?}"));
        assert!(v.get("content").is_some());
    }
}

#[test]
fn space_separated_ts_compared_correctly() {
    // space-separated timestamp must not be lexicographically mis-ordered
    let path = fixture(
        "2026-10-09 01:00:00 INFO early\n2026-10-09 07:00:00 INFO mid\n2026-10-09 09:00:00 INFO late\n",
    );
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--since", "2026-10-09T05:00:00"]);

    assert!(out.contains("mid"), "07:00 must pass a 05:00 bound; got: {out:?}");
    assert!(out.contains("late"));
    assert!(!out.contains("early"));
}

#[test]
fn space_separated_bounds_normalized_too() {
    // the BOUND must get the same space->T normalize as line timestamps,
    // else 'T' at byte 10 > ' ' mis-compares and the filter drops/keeps all
    let path = fixture("2026-10-09 05:00:00 INFO early\n2026-10-09 11:00:00 INFO late\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "2026-10-09 09:00:00"]);

    assert!(out.contains("early"), "got: {out:?}");
    assert!(!out.contains("late"), "got: {out:?}");
}

#[test]
fn apache_ts_lines_bypass_range_filter() {
    // Apache leading ts sorts backwards lexicographically — must NOT be
    // compared against ISO bounds, only ISO leading ts participates
    let path = fixture("09/Oct/2026:11:22:33 error disk full\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--since", "2026-01-01"]);

    assert!(out.contains("disk full"), "non-ISO ts must pass, got: {out:?}");
}

#[test]
fn bracket_prefixed_ts_is_range_filtered() {
    // timestamps behind a prefix (brackets, level) must still be compared
    let path = fixture("[2026-10-09T12:00:00Z INFO boot]\n[2026-10-10T12:00:00Z INFO later]\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--since", "2026-10-10"]);

    assert!(out.contains("later"), "got: {out:?}");
    assert!(!out.contains("boot"), "prefixed ts must be filtered, got: {out:?}");
}

#[test]
fn date_bounds_keep_end_day() {
    // date-only --until means next-day start: consecutive days keep both
    let path = fixture(
        "2026-10-08T10:00:00Z INFO d8\n2026-10-09T10:00:00Z INFO d9\n2026-10-10T10:00:00Z INFO d10\n",
    );
    let out = cardio(&[
        path.0.to_string_lossy().as_ref(),
        "--since", "2026-10-08", "--until", "2026-10-09",
    ]);

    assert!(out.contains("d8") && out.contains("d9"), "got: {out:?}");
    assert!(!out.contains("d10"), "got: {out:?}");
}

#[test]
fn date_until_rolls_month() {
    // next-day arithmetic across a month boundary, leap year Feb 28->29
    let path = fixture("2026-02-28T10:00:00Z INFO feb28\n2026-02-29T10:00:00Z INFO feb29\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "2026-03-01"]);

    assert!(out.contains("feb28") && out.contains("feb29"), "got: {out:?}");
}

#[test]
fn summary_keeps_ids_with_first_last() {
    // punto 2: per-ID map that PRESERVES ids, compact to first/last + count
    let path = fixture(
        "2026-10-09T12:00:00Z INFO req_42 start\n2026-10-09T12:00:01Z INFO req_42 mid\n2026-10-09T12:00:02Z INFO req_42 done\n2026-10-09T12:00:03Z INFO req_43 start\n",
    );
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--summary", "--trace", "req_([0-9]+)"]);

    assert!(out.contains("[42] 3 events"), "got: {out:?}");
    assert!(out.contains("req_42 start") && out.contains("req_42 done"), "got: {out:?}");
    assert!(!out.contains("req_42 mid"), "summary keeps only first/last, got: {out:?}");
    assert!(out.contains("[43] 1 event"), "got: {out:?}");
}

#[test]
fn summary_json_has_id_count_bounds() {
    let path = fixture("2026-10-09T12:00:00Z INFO req_42 start\n2026-10-09T12:00:02Z INFO req_42 done\n");
    let out = cardio(&[
        path.0.to_string_lossy().as_ref(),
        "--summary", "--trace", "req_([0-9]+)", "--json",
    ]);

    let lines: Vec<&str> = out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1);
    let v: serde_json::Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("summary line must be JSON: {e}"));
    assert_eq!(v["id"], "42");
    assert_eq!(v["count"], serde_json::json!(2));
    assert!(v["first"].as_str().unwrap().contains("start"));
    assert!(v["last"].as_str().unwrap().contains("done"));
}

#[test]
fn single_event_summary_prints_once() {
    let path = fixture("2026-10-09T12:00:03Z INFO req_43 only one\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--summary", "--trace", "req_([0-9]+)"]);

    assert!(out.contains("[43] 1 event"), "got: {out:?}");
    assert_eq!(out.matches("only one").count(), 1, "1-event session must print its line once, got: {out:?}");
}

#[test]
fn stats_plus_summary_emits_nothing() {
    // --stats is stderr-only survey: summary must not leak a phantom header
    let path = fixture("2026-10-09T12:00:00Z INFO req_42 start\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--stats", "--summary", "--trace", "req_([0-9]+)"]);

    assert_eq!(out.trim(), "", "got: {out:?}");
}

#[test]
fn summary_last_is_exactly_one_event() {
    // regression: the last event must be the line alone, never doubled
    let path = fixture(
        "2026-10-09T12:00:00Z INFO req_42 start\n2026-10-09T12:00:01Z INFO req_42 mid\n2026-10-09T12:00:02Z INFO req_42 done\n",
    );
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--summary", "--trace", "req_([0-9]+)"]);

    assert!(out.lines().any(|l| l == "req_42 done"), "got: {out:?}");
    assert!(!out.contains("donereq"), "got: {out:?}");
}

#[test]
fn bq_csv_rows_are_clean_events() {
    // fake bq on PATH: header row, quoted comma field, empty-text row.
    // Regression: comma join left ",..." residue + the empty row as phantom ","
    let fake = std::env::temp_dir()
        .join(format!("cardio_fakebin_{}", std::process::id()));
    std::fs::create_dir_all(&fake).expect("mkdir fakebin");
    let script = fake.join("bq");
    std::fs::write(&script, "#!/bin/sh\ncat <<'CSV'\ntimestamp,text\n2026-10-13T12:00:00Z,\"req_1 login, ok\"\n2026-10-13T12:00:02Z,\nCSV\n")
        .expect("write fake bq");
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make fake bq executable");

    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(["--bq", "SELECT timestamp, text FROM t", "--json"])
        .env("PATH", format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default()))
        .output()
        .expect("cardio binary should run");
    assert!(
        out.status.success(),
        "cardio exited {}: stderr: {:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(), 1,
        "header + empty-text row must not be events: {stdout:?}"
    );
    let v: serde_json::Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("line must be JSON: {e}"));
    assert_eq!(v["content"], "req_1 login, ok", "no separator residue: {stdout:?}");
    std::fs::remove_dir_all(&fake).ok();
}

#[test]
fn bq_crlf_text_field_flattens_to_one_line() {
    // quoted field with CRLF breaks: the event must stay one line
    let fake = std::env::temp_dir()
        .join(format!("cardio_fakebin_{}_crlf", std::process::id()));
    std::fs::create_dir_all(&fake).expect("mkdir fakebin");
    let script = fake.join("bq");
    std::fs::write(&script, "#!/bin/sh\nprintf 'timestamp,text\\n\"line1\\r\\nline2 more\",x\\n'\n")
        .expect("write fake bq");
    #[cfg(unix)]
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make fake bq executable");

    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(["--bq", "SELECT 1"])
        .env("PATH", format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default()))
        .output()
        .expect("cardio binary should run");
    std::fs::remove_dir_all(&fake).ok();
    assert!(
        out.status.success(),
        "cardio exited {}: stderr: {:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "CRLF field must flatten to one event: {stdout:?}");
    assert_eq!(lines[0], "line1 line2 more x", "no CR residue: {stdout:?}");
}

#[test]
fn minute_precision_ts_pads_to_seconds() {
    // minute-precision line ts must pad to ":00": unpadded it sorts as a bare
    // prefix strictly below its own minute's second-precision bounds
    // (fixable half: since >= at the exact minute, until < from it — the
    // second-precision --since ABOVE the minute is unknowable from the text)
    let cfg = fixture("timestamp_regex = '\\d{4}-\\d{2}-\\d{2} \\d{2}:\\d{2}'\n");
    let path = fixture("2026-10-09 12:00 event\n2026-10-09 11:00 old\n");
    let cfgarg = cfg.0.to_string_lossy().to_string();
    let patharg = path.0.to_string_lossy().to_string();

    let kept = cardio(&["--config", &cfgarg, "--since", "2026-10-09T12:00:00", &patharg]);
    assert!(kept.contains("event"), "12:00 (= 12:00:00) must pass --since, got: {kept:?}");
    assert!(!kept.contains("old"), "got: {kept:?}");

    let dropped = cardio(&["--config", &cfgarg, "--until", "2026-10-09T12:00:00", &patharg]);
    assert!(!dropped.contains("event"), "exclusive --until drops the exact minute, got: {dropped:?}");
    assert!(dropped.contains("old"), "11:00 stays behind the bound, got: {dropped:?}");
}

#[test]
fn date_only_ts_passes_its_own_since_day() {
    // a date-only line ts is a strict prefix of the canonical bound 'YYYY-
    // MM-DDT00:00:00' — it must compare as that day's start, not get dropped
    let cfg = fixture("timestamp_regex = '\\d{4}-\\d{2}-\\d{2}'\n");
    let path = fixture("2026-10-09 boot\n2026-10-10 later\n");
    let out = cardio(&[
        "--config", cfg.0.to_string_lossy().as_ref(),
        "--since", "2026-10-09T00:00:00",
        "--until", "2026-10-10T00:00:00",
        path.0.to_string_lossy().as_ref(),
    ]);

    assert!(out.contains("boot"), "same-day line must pass --since, got: {out:?}");
    assert!(!out.contains("later"), "got: {out:?}");
}

#[test]
fn impossible_date_bound_is_rejected() {
    // calendar sanity: 2026-99-99 must be warned about, not silently trusted
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(["--until", "2026-99-99"])
        .output()
        .expect("cardio binary should run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a full ISO"), "got: {err:?}");
}