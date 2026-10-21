//! The cross-module tuning knobs, one home. Per-module-only constants
//! (DEDUP_EXACT_CAP, MAX_TRACE_SESSIONS...) live next to their only user.

/// lean window size; the SIDE depends on the source (fetcher = head,
/// stream = tail) — the window policy in main pairs them
pub(crate) const LEAN_MAX_LINES: usize = 2_000;

/// lean default field cut: JWTs/payloads that never carry an id in their
/// tail get ellipsis-marked instead of eating context; 0 disables
pub(crate) const FIELD_LEN_DEFAULT: usize = 1000;

/// --stats survey cap: unique patterns tracked before the counter stops
pub(crate) const MAX_STATS_PATTERNS: usize = 100_000;

/// --timeout default for fetchers: the 20-minute silent bq was real;
/// agents should never have to remember the guard exists. 0 disables.
pub(crate) const FETCH_TIMEOUT_DEFAULT: u64 = 600;