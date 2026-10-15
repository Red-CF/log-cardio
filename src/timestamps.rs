//! Timestamp grammar + canonical compare. THE single ts-grammar home:
//! validation, canonicalization, bounds and server-pushdown literals all
//! live here; every compare lands on the `to_canonical` shape asserted by
//! `assert_canonical_shape`.

use regex::Regex;
use std::sync::LazyLock;

/// A frac may only follow SECONDS: a frac on minutes is not a real log
/// grammar and is rejected like any other invalid ts.
/// [0-9] over \d: regex \d is Unicode-aware and every byte check
/// downstream (is_full_iso arithmetic, all_digits, truncate, shape
/// gate) assumes ASCII digits — custom configs carrying \d re-open
/// that leak at their own trim layer.
pub(crate) static FRAC: &str = r"\.[0-9]+";

/// The unanchored full-ISO body — THE one ts-grammar home for
/// validation AND (below, as TRIM_BODY) the default trim regex. Widen
/// FRAC here (comma frac, etc.) and both follow.
///
/// ponytail: structural and
/// calendar-sanity only — exact days-in-month and tz semantics stay
/// out (chrono if needed).
pub(crate) static ISO_BODY: LazyLock<String> = LazyLock::new(|| {
    format!(
        r"[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}(?:[ T][0-9]{{2}}:[0-9]{{2}}(?::[0-9]{{2}}(?:{FRAC})?)?(?:Z|[+-][0-9]{{2}}:?[0-9]{{2}})?)?"
    )
});

static FULL_ISO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!("^(?:{})$", *ISO_BODY)).unwrap()
});

/// The default trim grammar: TRIM_BODY = ISO_BODY with the time part
/// made MANDATORY (a bare date can be content — "deployed on
/// 2026-10-09" — and would be corrupted if trim stripped it; such
/// lines bypass the range filter with the usual one-shot warn, which
/// is the documented fallback). Seconds now optional: trim previously
/// required them, so a valid minute-precision ts never reached the
/// range gate and the line leaked past --since/--until (verified).
/// The Apache month is [A-Za-z], not \w — \w is Unicode-aware and its
/// match is trimmed-but-unvalidatable, the leak class above.
/// ponytail: a mixed ascii/unicode ts match stops at the fullwidth
/// char and leaves a small fragment in the line — cosmetic; the old
/// \d grammar stripped the token whole but re-opened that leak.
pub(crate) static TRIM_BODY: LazyLock<String> = LazyLock::new(|| {
    format!(
        r"[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}}[ T][0-9]{{2}}:[0-9]{{2}}(?::[0-9]{{2}}(?:{FRAC})?)?(?:Z|[+-][0-9]{{2}}:?[0-9]{{2}})?"
    )
});

/// Exactly MAX_FRAC frac digits on every compare — every instant emitted
/// by to_canonical carries the full run, frac-less forms included (a bare
/// "…45" is a strict prefix of "…45.000…" and would mis-order otherwise).
/// 19 covers real (≤ nanosecond) and pathological ≤19; longer fracs
/// truncate, so >19-digit fracs COLLAPSE: distinct instants compare equal
/// (extra keeps on --since, one final bucket lost on --until, line-vs-line
/// order blurs). Compare numerically via chrono if that ever matters.
pub(crate) const MAX_FRAC: usize = 19;
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

pub(crate) fn is_full_iso(s: &str) -> bool {
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

/// THE single canonical-for-compare writer: space→'T' (" " < "T" would
/// mis-order), the tz suffix strips whole (compare is tz-naive — a '+'
/// or 'Z' tail itself would mis-order equal instants), date-only = day
/// start, minute precision pads to ":00", and every instant ends with
/// MAX_FRAC frac digits. Lines and bounds both land here — one
/// invariant, no drift, asserted by assert_canonical_shape at BOTH exits
/// (any drift anywhere in the pipeline panics loud, never mis-compares).
pub(crate) fn to_canonical(s: &str, out: &mut String) {
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
        // minute-precision form ("T24:00", len 16) — the only reason for
        // the len > 19 guard (that form used to panic as a raw slice
        // OOB). split_off cuts the tail off in place: the None branch
        // discards it, the Some branch re-pushes it verbatim.
        // ponytail: no residual grammar-check — FULL_ISO_RE already
        // validated the input, and its EOD forms leave only an empty or
        // '.'-digit tail; a junk tail is caught by the end shape gate
        // (Some branch) or discarded harmlessly by the clamp (None).
        // Ceiling: widening the EOD grammar must add a check back here,
        // not more asserts.
        let residual = if out.len() > 19 { out.split_off(19) } else { String::new() };
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
    let res = {
        let b = out.as_bytes();
        out.len() >= 16 && b[10] == b'T' && b[13] == b':' && b.get(16) != Some(&b':')
    }; if res {
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
pub(crate) fn normalize_iso<'a>(ts: &'a str, buf: &'a mut String) -> &'a str {
    to_canonical(ts, buf);
    buf.as_str()
}

/// Bounds canonicalize via to_canonical (space→T, padding, frac width).
/// --until is <, so a DATE-ONLY bound means NEXT-day start: "--until 2026-10-09"
/// keeps all of the 9th instead of excluding the whole end day. Padded
/// precisions keep their instant (exclusive at the pad instant).
/// ponytail: bounds normalized at this layer so callers can't forget the
/// convention; offset handling stays tz-naive — chrono if that ever matters.
pub(crate) fn canonical_bound(s: &str, until: bool) -> String {
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
pub(crate) fn next_day(date: &str) -> Option<String> {
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
pub(crate) fn max_instant(out: &mut String) {
    to_canonical("9999-12-31T23:59:59", out); // gate-checked: '.' at 19
    // ZEROS → NINES: digit-for-digit swap of the MAX_FRAC-width run
    // (width compile-time-pinned) over the to_canonical-gated shape —
    // safe by construction, no runtime net.
    out.replace_range(20.., NINES);
}

/// Canonical-shape gate: to_canonical's ONLY legal output — full datetime,
/// len 39 (19 + '.' + MAX_FRAC), separators pinned, every other byte a
/// digit. ONE gate at every to_canonical exit covers every compare (bounds
/// and per-line alike) and every pushdown embedder; drift panics loud,
/// naming the string.
pub(crate) fn assert_canonical_shape(canonical: &str) {
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
pub(crate) fn until_server_bound(canonical: &str) -> Option<String> {
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
pub(crate) fn server_ts(canonical: &str) -> &str {
    assert_canonical_shape(canonical);
    &canonical[..19]
}