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

#[cfg(unix)]
#[test]
fn bq_csv_rows_are_clean_events() {
    // header row, quoted comma field, empty-text row: no separator residue,
    // no phantom "," event
    let stdout = fake_bq(
        "timestamp,text\n2026-10-13T12:00:00Z,\"req_1 login, ok\"\n2026-10-13T12:00:02Z,\n",
        &["--bq", "SELECT timestamp, text FROM t", "--json"],
    );
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "header + empty-text row are not events: {stdout:?}");
    let v: serde_json::Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("line must be JSON: {e}"));
    assert_eq!(v["content"], "req_1 login, ok", "no separator residue: {stdout:?}");
}

#[cfg(unix)]
#[test]
fn bq_crlf_text_field_flattens_to_one_line() {
    // quoted field with CRLF breaks: the event must stay one line
    let stdout = fake_bq(
        "timestamp,text\n\"line1\r\nline2 more\",x\n",
        &["--bq", "SELECT 1"],
    );
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "CRLF field must flatten to one event: {stdout:?}");
    assert_eq!(lines[0], "line1 line2 more\tx", "no CR residue: {stdout:?}");
}

#[cfg(unix)]
#[test]
fn bq_lone_cr_content_is_preserved() {
    // CR flattening must only eat CRLF pairs, never a lone CR's content
    let stdout = fake_bq("timestamp,text\n\"code:\r17\",x\n", &["--bq", "SELECT 1"]);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "lone CR stays inside one event: {stdout:?}");
    assert_eq!(lines[0], "code:\r17\tx", "no content byte destroyed: {stdout:?}");
}

/// Run cardio with a fake `bq` command on PATH that just prints `csv`.
/// (unix-only: the fake is a shebang shell script.)
/// Returns stdout; asserts exit status so a regression failing early is
/// honest. Fakebin dir is unique per call and removed before returning.
#[cfg(unix)]
fn fake_bq(csv: &str, args: &[&str]) -> String {
    use std::os::unix::fs::PermissionsExt;
    let fake = std::env::temp_dir().join(format!(
        "cardio_fakebin_{}",
        FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&fake).expect("mkdir fakebin");
    let script = fake.join("bq");
    std::fs::write(&script, format!("#!/bin/sh\ncat <<'CARDIO_CSV'\n{csv}\nCARDIO_CSV\n"))
        .expect("write fake bq");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make fake bq executable");
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(args)
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
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn invalid_first_ts_does_not_bypass_bounds() {
    // the first ts match being invalid (bad calendar date) must not shield
    // the line: a full-ISO ts later in the line is the compare target
    let path = fixture("x 2026-99-99T10:00:00 boot 2026-10-09T09:00:00 ok\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "2026-10-09T08:00:00"]);

    assert!(!out.contains("boot"), "line must use its valid ts, got: {out:?}");
}

#[test]
fn fractional_seconds_compare_equal_at_bound() {
    // '45.5' (== 45.500000000) must not sort below '45.500' as a bare prefix
    let path1 = fixture("2026-10-09T14:30:45.5 event\n");
    let path2 = fixture("2026-10-09T14:30:45.5 event\n");

    let dropped = cardio(&[path1.0.to_string_lossy().as_ref(), "--until", "2026-10-09T14:30:45.500"]);
    assert!(!dropped.contains("event"), "equal instant fails the exclusive bound, got: {dropped:?}");

    let kept = cardio(&[path2.0.to_string_lossy().as_ref(), "--since", "2026-10-09T14:30:45.500"]);
    assert!(kept.contains("event"), "equal instant passes the inclusive bound, got: {kept:?}");
}

#[test]
fn long_fraction_bound_truncates_comparable() {
    // >9 frac digits: the bound truncates to 9, equal instants compare equal
    let p1 = fixture("2026-10-09T12:34:45.1 event\n");
    let p2 = fixture("2026-10-09T12:34:45.1 event\n");
    let s1 = p1.0.to_string_lossy().into_owned();
    let s2 = p2.0.to_string_lossy().into_owned();

    let kept = cardio(&["--since", "2026-10-09T12:34:45.1000000000", &s1]);
    assert!(kept.contains("event"), "equal instant passes inclusive bound, got: {kept:?}");
    let dropped = cardio(&["--until", "2026-10-09T12:34:45.1000000000", &s2]);
    assert!(!dropped.contains("event"), "equal instant fails exclusive bound, got: {dropped:?}");
}

#[test]
fn tz_suffixed_minute_bound_pads_to_seconds() {
    // "T12:34Z" is minute precision: pads to ":00" before the suffix,
    // else ':' (0x3A) < 'Z' (0x5A) mis-orders every second-precision line
    let p1 = fixture("2026-10-09T12:34:30 event\n");
    let p2 = fixture("2026-10-09T12:34:30 event\n");
    let s1 = p1.0.to_string_lossy().into_owned();
    let s2 = p2.0.to_string_lossy().into_owned();

    let kept = cardio(&["--since", "2026-10-09T12:34Z", &s1]);
    assert!(kept.contains("event"), "mid-minute line passes the minute bound, got: {kept:?}");
    let dropped = cardio(&["--until", "2026-10-09T12:34Z", &s2]);
    assert!(!dropped.contains("event"), "mid-minute line must not cross the bound, got: {dropped:?}");
}

#[test]
fn until_long_fraction_keeps_before_bucket() {
    // fracs pad to MAX_FRAC: the exclusive bound must not swallow any
    // bucket below it (.9999999995 is strictly before .9999999999)
    let path = fixture("2026-10-09T12:34:45.9999999995 event\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "2026-10-09T12:34:45.9999999999"]);

    assert!(out.contains("event"), "got: {out:?}");
}

#[test]
fn over_max_frac_digits_truncate_and_still_order() {
    // >MAX_FRAC run: the truncation branch (out.truncate) must keep
    // relative order of the surviving 19 digits, not drop everything
    let path = fixture("2026-10-09T12:34:45.9999999999999999995 event\n");
    let out = cardio(&[
        path.0.to_string_lossy().as_ref(),
        "--until", "2026-10-09T12:34:45.99999999999999999999",
    ]);

    assert!(out.contains("event"), "truncated ..995 stays below the bound, got: {out:?}");
}

#[test]
fn non_zero_offset_strips_naive() {
    // tz-naive strips EVERY offset suffix, not only zero ones: '…13:00:00
    // +01:00' and '…13:00:00Z'/'…13:00:00' are then the same instant
    let path = fixture("2026-10-09T13:00:00+01:00 event\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "2026-10-09T13:00:00"]);

    assert!(!out.contains("event"), "naive equal instant fails the exclusive bound, got: {out:?}");
}

#[test]
fn zero_offset_and_z_compare_equal() {
    // '+00:00' and 'Z' are the same instant: suffixes must not mis-order
    // ('+' 0x2B < 'Z' 0x5A would drop the line from an equal --since)
    let p1 = fixture("2026-10-09T12:34:45+00:00 event\n");
    let p2 = fixture("2026-10-09T12:34:45+00:00 event\n");
    let p3 = fixture("2026-10-09T12:34:45Z event\n");
    let s1 = p1.0.to_string_lossy().into_owned();
    let s2 = p2.0.to_string_lossy().into_owned();
    let s3 = p3.0.to_string_lossy().into_owned();

    let kept = cardio(&["--since", "2026-10-09T12:34:45Z", &s1]);
    assert!(kept.contains("event"), "equal instant passes inclusive bound, got: {kept:?}");
    let dropped = cardio(&["--until", "2026-10-09T12:34:45Z", &s2]);
    assert!(!dropped.contains("event"), "equal instant fails exclusive bound, got: {dropped:?}");
    let dropped2 = cardio(&["--until", "2026-10-09T12:34:45+00:00", &s3]);
    assert!(!dropped2.contains("event"), "symmetric case (Z line, +00:00 bound), got: {dropped2:?}");
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
    // calendar sanity: 2026-99-99 fails fast — an unusable bound must not
    // stream an unfiltered GB log
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(["--until", "2026-99-99"])
        .output()
        .expect("cardio binary should run");
    assert!(!out.status.success(), "invalid bound must fail fast");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a full ISO"), "got: {err:?}");
}

#[test]
fn out_of_range_time_bound_is_rejected() {
    // hour/minute sanity: T99:99:99 would mis-order locally and be
    // unembeddable server-side — fail fast like a bad date
    for b in ["2026-10-09T99:99:99", "2026-10-09T12:99:00", "2026-10-09T24:00.5"] {
        let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
            .args(["--since", b])
            .output()
            .expect("cardio binary should run");
        assert!(!out.status.success(), "{b} must fail fast, got: {:?}", out.status);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not a full ISO"), "{b}: got: {err:?}");
    }
}

#[test]
fn iso_end_of_day_24_00_compares_as_next_day() {
    // T24:00:00 is legal ISO (end of day) and must canonicalize to the
    // next day's 00:00 — left alone it would sort after everything while
    // sitting before the next day's instants
    let path = fixture("2026-10-09T20:00:00Z early\n2026-10-09T24:00:00Z boot\n");
    let p = path.0.to_string_lossy().into_owned();

    let kept = cardio(&["--since", "2026-10-09T20:00:00", &p]);
    assert!(kept.contains("boot") && kept.contains("early"), "got: {kept:?}");

    // boot == next-day 00:00 == the exclusive until exactly, so it drops;
    // early's instant is strictly before
    let dropped = cardio(&["--until", "2026-10-10T00:00:00", &p]);
    assert!(dropped.contains("early"), "got: {dropped:?}");
    assert!(!dropped.contains("boot"), "24:00 == next-day 00:00 instant, got: {dropped:?}");
}

#[test]
fn minute_precision_eod_bound_clamps_to_next_day() {
    // "T24:00" (len 16, no seconds) is the same rewrite path — a slice
    // instead of .get() panicked here with a raw OOB (regression r21).
    // The bound must canonicalize to next-day 00:00 exactly like the
    // seconds form.
    let path = fixture("2026-10-09T20:00:00 boot\n2026-10-10T00:00:00 up\n");
    let p = path.0.to_string_lossy().into_owned();

    let kept = cardio(&["--since", "2026-10-09T24:00", &p]);
    assert!(kept.contains("up") && !kept.contains("boot"), "got: {kept:?}");

    let dropped = cardio(&["--until", "2026-10-09T24:00Z", &p]);
    assert!(dropped.contains("boot") && !dropped.contains("up"), "got: {dropped:?}");
}

#[test]
fn minute_precision_line_enters_range_filter() {
    // verified leak (r24): trim required seconds, so a valid minute-
    // precision ts never reached is_full_iso and the line past --since
    // printed unchecked; the default trim grammar now matches it
    let path = fixture("2026-10-01T05:00 early thing\n2026-10-13T05:00 late thing\n");
    let p = path.0.to_string_lossy().into_owned();
    let out = cardio(&["--since", "2026-10-09", &p]);
    assert!(out.contains("late") && !out.contains("early"), "got: {out:?}");
}

#[test]
fn agent_md_prints_card() {
    // the card must stay honest: exit 0, name the tool and the flags
    // the agent actually needs (lean mode, the dedup warning, ranges)
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .arg("--agent-md")
        .output()
        .expect("cardio binary should run");
    assert!(out.status.success(), "got: {:?}", out.status);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("lean by default") && s.contains("UNIQUE") && s.contains("--raw") && s.contains("--since"),
        "got: {s:?}"
    );
}

#[test]
fn default_pipeline_dedups_and_caps_raw_skips_it() {
    // lean is the default: dedup on + --max-lines 2000 (overridable);
    // --raw = no dedup, no cap. Repeats must collapse and the cap must
    // apply without any flag
    let mut content = String::new();
    content.push_str("2026-10-09T12:00:00 repeated start\n");
    for i in 0..2100 {
        content.push_str(&format!("2026-10-09T12:{:02}:{:02} unique-{i:04}\n", i / 60, i % 60));
    }
    content.push_str("2026-10-09T23:00:00 repeated start\n");
    let path = fixture(&content);
    let p = path.0.to_string_lossy().into_owned();

    let out = cardio(&[&p]);
    // the two "repeated start" lines collapse to the first (dedup) — but
    // the window is the LAST 2000 KEPT lines: the head (kept once by dedup,
    // then unique-0000..0099) slid OUT, recent beats old
    let count = out.matches("repeated start").count();
    assert_eq!(count, 0, "head of the range must leave the window, got {count}");
    assert!(out.contains("unique-2099") && !out.contains("unique-0099"), "cap 2000 must keep the most recent");
    // explicit --max-lines overrides the default cap (same tail semantics)
    let capped = cardio(&["--max-lines", "5", &p]);
    assert_eq!(capped.lines().count(), 5, "got {}", capped.lines().count());
    assert!(capped.contains("unique-2099"), "explicit cap keeps the latest: {capped:?}");
    // --raw: no dedup, no cap — both repeats and every unique survive
    let raw = cardio(&["--raw", &p]);
    assert_eq!(raw.matches("repeated start").count(), 2, "--raw must keep repeats");
    assert!(raw.contains("unique-2099"), "--raw must keep the tail");
}

#[test]
fn tail_keeps_last_n_after_dedup() {
    // --tail N = last N KEPT lines (post-dedup): window slides, cap not
    // applied, whole input read; --raw changes nothing about the window
    let mut content = String::new();
    content.push_str("2026-10-09T12:00:00 repeated start\n");
    for i in 0..2100 {
        content.push_str(&format!("2026-10-09T12:{:02}:{:02} unique-{i:04}\n", i / 60, i % 60));
    }
    content.push_str("2026-10-09T23:00:00 repeated start\n");
    let path = fixture(&content);
    let p = path.0.to_string_lossy().into_owned();

    let out = cardio(&["--tail", "5", &p]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "got {}", lines.len());
    // the window is the last 5 KEPT lines; the trailing "repeated start"
    // was already dropped by dedup before the window saw it
    assert!(out.contains("unique-2099"), "latest line kept: {out:?}");
    assert!(!out.contains("unique-0000") && !out.contains("repeated start"), "early lines excluded: {out:?}");
    // json indices still reference the input line numbers
    let j = cardio(&["--tail", "1", "--json", &p]);
    assert!(j.contains("\"index\":") && j.lines().count() == 1, "got: {j:?}");
}

#[test]
fn lean_signals_what_it_dropped() {
    // the default pipeline never drops silently: stderr must say how
    // many suspect duplicates were removed and that the cap cut the run
    let path = fixture("2026-10-09T12:00:00 dup\n2026-10-09T12:00:01 dup\n2026-10-09T12:00:02 keep\n");
    let p = path.0.to_string_lossy().into_owned();
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args([&p])
        .output()
        .expect("cardio binary should run");
    assert!(out.status.success(), "got: {:?}", out.status);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("dedup dropped 1 duplicate-suspect line"), "got: {err:?}");
    assert!(!err.contains("kept the last"), "no cap in a short run: {err:?}");

    let big: String = (0..2001).map(|i| format!("2026-10-09T12:{:02}:{:02} u{i:04}\n", i / 60, i % 60)).collect();
    let path = fixture(&big);
    let p = path.0.to_string_lossy().into_owned();
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args([&p])
        .output()
        .expect("cardio binary should run");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("kept the last 2000 lines"), "cap signal missing: {err:?}");
    assert!(!err.contains("the rest of the input was not read"), "no early-cut in lean: {err:?}");
}

#[test]
fn stats_json_emits_patterns_on_stdout() {
    let path = fixture("2026-10-09T12:00:00 alpha\n2026-10-09T12:00:01 beta\n2026-10-09T12:00:02 alpha\n");
    let p = path.0.to_string_lossy().into_owned();
    let out = cardio(&["--stats", "--json", &p]);
    // NDJSON on stdout, patterns from the TRIMMED line bodies
    assert!(out.contains("\"count\":2") && out.contains("alpha"), "got: {out:?}");
    assert!(out.contains("\"count\":1") && out.contains("beta"), "got: {out:?}");
}

#[test]
fn frac_on_minutes_bound_is_rejected() {
    // a frac on minutes is no real grammar — rejected like any invalid ts
    let path = fixture("2026-10-09T12:34:30 event\n");
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args([path.0.to_string_lossy().as_ref(), "--since", "2026-10-09T12:34.5"])
        .output()
        .expect("cardio binary should run");
    assert!(!out.status.success(), "invalid bound must fail fast");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a full ISO"), "got: {err:?}");
}

#[test]
fn last_year_bound_keeps_end_day() {
    // --until 9999-12-31 must keep the whole day: the wrapped "10000-…"
    // string would sort BEFORE "9999-…", the sentinel sorts above it
    let path = fixture("9999-12-31T22:00:00Z late\n9999-12-30T10:00:00Z earlier\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "9999-12-31"]);

    assert!(out.contains("late"), "end day survives the last --until, got: {out:?}");
    assert!(out.contains("earlier"), "got: {out:?}");
}

#[test]
fn over_max_frac_line_collapses_to_max_shape() {
    // 20-digit line fracs truncate to MAX_FRAC: their first 19 digits must
    // still order the line — both sit just under 12:34:46, and both collapse
    // onto the same 19-digit instant (documented ceiling: >19-digit fracs
    // line-vs-line order blurs)
    let path = fixture(
        "2026-10-09T12:34:45.99999999999999999995 short\n2026-10-09T12:34:45.99999999999999999999 long\n2026-10-09T12:34:46 later\n",
    );
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "2026-10-09T12:34:46"]);

    assert!(out.contains("short"), "20-digit ..995 stays under :46, got: {out:?}");
    assert!(out.contains("long"), "20-digit ..999 stays under :46, got: {out:?}");
    assert!(!out.contains("later"), ":46.0 fails the exclusive bound, got: {out:?}");

    let out = cardio(&[
        path.0.to_string_lossy().as_ref(),
        "--until", "2026-10-09T12:34:45.9999999999999999999",
    ]);
    assert!(
        !out.contains("short") && !out.contains("long"),
        "both truncations collapse onto the equal instant, got: {out:?}"
    );
}

/// Run cardio with a fake `gcloud` on PATH that echoes its FILTER arg
/// (3rd positional, after `logging read`) to STDERR — cardio's stdout
/// would trim the ts out of it via the normal pipeline. Asserts exit
/// status; unique fakebin dir, removed before returning.
/// Same as `cardio`, but both channels come back separately.
fn cardio_out(args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(args)
        .output()
        .expect("cardio binary should run");
    assert!(
        out.status.success(),
        "cardio exited {}: stderr: {:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[cfg(unix)]
/// Fake bq whose argv is echoed to stderr (ARG:<flag> lines) and whose
/// stdout is a fixed CSV — lets tests pin the exact bq invocation.
fn fake_bq_argv(csv: &str, args: &[&str]) -> (String, String) {
    use std::os::unix::fs::PermissionsExt;
    let fake = std::env::temp_dir().join(format!(
        "cardio_fakebqa_{}",
        FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&fake).expect("mkdir fakebin");
    let script = fake.join("bq");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do echo \"ARG:$a\" >&2; done\ncat <<'CARDIO_CSV'\n{csv}\nCARDIO_CSV\n"
        ),
    )
    .expect("write fake bq");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make fake bq executable");
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(args)
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
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn raw_is_verbatim_timestamps_and_levels_survive() {
    let path = fixture("2026-10-09T12:00:00Z INFO User login ok\n\n2026-10-09T12:00:01Z app boom\n");
    let (out, _) = cardio_out(&[path.0.to_string_lossy().as_ref(), "--raw"]);
    assert!(out.contains("2026-10-09T12:00:00Z"), "--raw must not strip timestamps: {out:?}");
    assert!(out.contains("INFO"), "--raw must not strip boilerplate: {out:?}");
    assert!(out.contains("\n\n"), "--raw keeps blank lines: {out:?}");

    let (lean, _) = cardio_out(&[path.0.to_string_lossy().as_ref()]);
    assert!(!lean.contains("2026-10-09T12:00:00Z"), "lean strips the timestamp: {lean:?}");
}

#[cfg(unix)]
#[test]
fn bq_argv_carries_cost_and_project_flags() {
    let (_, err) = fake_bq_argv(
        "timestamp,text\nt1,hello\n",
        &[
            "--bq", "SELECT 1",
            "--project", "other-project",
            "--dry-run",
            "--max-bytes-billed", "1111",
            "--bq-args=--location=eu",
        ],
    );
    for flag in ["--quiet", "--project_id=other-project", "--dry_run", "--maximum_bytes_billed=1111", "--location=eu"] {
        assert!(err.contains(&format!("ARG:{flag}")), "bq argv missing {flag}: {err:?}");
    }
    assert!(err.contains("Note: bq columns: timestamp | text"), "columns note missing: {err:?}");
    // default run: no project, no dry-run, but --quiet always on
    let (_, err) = fake_bq_argv("timestamp,text\nt1,hello\n", &["--bq", "SELECT 1"]);
    assert!(err.contains("ARG:--quiet"), "default bq argv must be quiet: {err:?}");
    assert!(!err.contains("project_id"), "no project flag by default: {err:?}");
    assert!(!err.contains("dry_run"), "no dry-run by default: {err:?}");
}

#[test]
fn bq_flags_error_without_bq() {
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(["--dry-run"])
        .output()
        .expect("cardio binary should run");
    assert!(!out.status.success(), "--dry-run without --bq must fail loudly");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("only works with --bq"), "usage error missing: {err:?}");
}

#[cfg(unix)]
fn fake_gcloud(args: &[&str]) -> String {
    let fake = std::env::temp_dir().join(format!(
        "cardio_fakegc_{}",
        FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&fake).expect("mkdir fakebin");
    let script = fake.join("gcloud");
    std::fs::write(&script, "#!/bin/sh\necho \"$3\" >&2\n")
        .expect("write fake gcloud");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("make fake gcloud executable");
    let out = Command::new(env!("CARGO_BIN_EXE_cardio"))
        .args(args)
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
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[cfg(unix)]
#[test]
fn gcloud_filter_embeds_plain_seconds() {
    // pushdown is second-precision: since >= its floor second, until <
    // the NEXT second (a <= floor literal would exclude (floor, frac)
    // entries the local filter keeps — "…45" means instant 45.0, not the
    // second). No frac shape ever reaches the fetcher.
    let echo = fake_gcloud(&[
        "--gcloud", "resource.type=x", "--since", "2026-10-09", "--until", "2026-10-09T12:34:45.5",
    ]);
    assert!(echo.contains("timestamp>=\"2026-10-09T00:00:00\""), "got: {echo:?}");
    assert!(echo.contains("timestamp<\"2026-10-09T12:34:46\""), "until embeds < next second");
    assert!(!echo.contains("=0000000000"), "bare 19-digit pad leaked to the server: {echo:?}");
    assert!(!echo.contains(".5\""), "frac leaked to the server literal: {echo:?}");
}

#[cfg(unix)]
#[test]
fn gcloud_last_year_until_skips_embed() {
    // past 9999-12-31 there is no next second: the until clause is
    // dropped server-side and the local exact filter bounds the stream
    let echo = fake_gcloud(&["--gcloud", "resource.type=x", "--until", "9999-12-31"]);

    assert!(echo.contains("resource.type=x"), "got: {echo:?}");
    assert!(!echo.contains("timestamp<"), "no next second -> no until embed, got: {echo:?}");
}

#[cfg(unix)]
#[test]
fn gcloud_until_day_rollover_embeds_next_day() {
    // regression: 23:59:59 until rolls s→m→h→day; the old code re-suffixed
    // next_day's full datetime → "…T00:00:00T00:00:00" garbage literal
    let echo = fake_gcloud(&["--gcloud", "resource.type=x", "--until", "2026-10-09T23:59:59"]);

    assert!(
        echo.contains("timestamp<\"2026-10-10T00:00:00\""),
        "day-roll embeds one well-formed literal, got: {echo:?}"
    );
    assert!(!echo.contains("T00:00:00T"), "double datetime suffix leaked: {echo:?}");
}

#[test]
fn last_year_final_second_fracs_survive_until() {
    // the 9999 clamp's frac is nines, not zeros: an until at the last day
    // must admit entries inside the final second (.000 pad would drop .5)
    let path = fixture("9999-12-31T23:59:59.5 last instant\n9999-12-31T12:00:00 day\n");
    let out = cardio(&[path.0.to_string_lossy().as_ref(), "--until", "9999-12-31"]);

    assert!(out.contains("day"), "got: {out:?}");
    assert!(out.contains("last instant"), "mid-second instant stays under the clamp, got: {out:?}");
}
