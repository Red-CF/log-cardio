//! The exit contract in ONE place: an error kind, its exit code, one
//! bail. stderr text stays plain (agents grep it), but "which code and
//! why" is now type-checked instead of scattered `exit(N)` calls.

pub(crate) enum Fail {
    /// misuse: bad bounds, wrong source pairing — usage errors exit 2
    Usage(String),
    /// I/O / config / input problems — exit 1
    Source(String),
    /// a fetcher child failed its own way — exit 1
    Fetch(String),
}

impl Fail {
    fn code(&self) -> i32 {
        match self {
            Fail::Usage(_) => 2,
            Fail::Source(_) | Fail::Fetch(_) => 1,
        }
    }
    fn text(&self) -> &str {
        match self {
            Fail::Usage(m) | Fail::Source(m) | Fail::Fetch(m) => m,
        }
    }
}

/// Print and exit with the kind's code — the single exit door for
/// hard failures (broken-pipe quits stay in pipeline's write path).
pub(crate) fn bail(f: Fail) -> ! {
    eprintln!("Error: {}", f.text());
    std::process::exit(f.code())
}