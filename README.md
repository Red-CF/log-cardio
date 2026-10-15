# log-cardio 🏃🏼‍♂️

[![CI](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml/badge.svg)](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml)
[![coverage](https://raw.githubusercontent.com/Red-CF/log-cardio/gh-pages/coverage.svg)](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/log-cardio.svg)](https://crates.io/crates/log-cardio)
[![macOS](https://img.shields.io/badge/macOS-supported-999999.svg)](https://github.com/Red-CF/log-cardio/releases)
[![Linux](https://img.shields.io/badge/Linux-supported-fcc624.svg)](https://github.com/Red-CF/log-cardio/releases)
[![Windows](https://img.shields.io/badge/Windows-supported-0078d6.svg)](https://github.com/Red-CF/log-cardio/releases)

Fast Rust CLI that trims the fat from logs — fewer tokens for LLM agents.
Logs can be GBs: cardio streams line by line with near-constant memory
(Bloom-filter dedup, capped stats/trace maps). Agents can fetch logs
themselves (`--gcloud`, `--bq`) and get context with `cardio --agent-md`.

Currently is in a WIP state, mainly because the project is startig to make it's firsts steps, the test outcomes are currently succesfull, & soon I will try to benchmark the tool thoroughly.

## Install

**With cargo** (installs the `cardio` binary):

```bash
cargo install log-cardio
```

**Without cargo** (macOS/Linux — downloads the binary from the latest release):

```bash
curl -fsSL https://raw.githubusercontent.com/Red-CF/log-cardio/main/install.sh | sh
```

**Windows**: grab the `cardio-Windows-X64.exe` from the [releases page](https://github.com/Red-CF/log-cardio/releases) and put it on your `PATH`.

Or from source:

```bash
cargo install --path .
```

## First run — getting oriented

Cardio ships its own context: the first thing any agent (or person) should
do is read the card, then survey the log — not read it raw.

```bash
cardio --agent-md > AGENTS.md   # the full contract: flags, output shapes, gotchas
cardio app.log --stats          # survey: what does the log even contain?
cardio app.log                  # then the lean read of what matters
```

Pipe `AGENTS.md` to your repo root once: agent runtimes pick it up
automatically and never re-learn the tool by trial and error.

## Tests

```bash
cargo test    # 61 total: 55 CLI integration (tests/cli.rs) + unit (src/timestamps.rs)
```

**Philosophy: black-box at the CLI level.** Tests pin the *promises* the
agent card makes (trim behavior, dedup honesty, exit codes, stderr
witnesses, JSON shapes) — so they survive refactors, and any doc-vs-code
drift fails loudly. gcloud/bq are faked with PATH-stub shell scripts
(`#[cfg(unix)]`) so the whole suite runs offline everywhere. Unit tests
exist only for pure math a CLI test can't probe sanely (calendar
rollovers, the 9999 clamp).

## Input

```bash
cardio app.log                              # file — lean by default (trim + dedup + cap 2000)
cardio --gcloud 'resource.type=gce_instance'
cardio --bq 'SELECT FORMAT_TIMESTAMP("%Y-%m-%dT%H:%M:%SZ", ts) AS timestamp, text FROM prod.traces'
cardio --raw app.log                        # fidelity: no dedup, no cap
```

> **⚠ The default pipeline drops lines.** Repeated lines are removed
> (Bloom filter — **~1% false positives: a unique line can be discarded
> as if repeated**) and only the **last** 2000 lines are kept — the most
> recent; the head of the range is trimmed (`--max-lines` overrides). It's
> the right trade for trimming noise; when any line might be THE answer,
> run `--raw` or narrow with `--query`.

gcloud mode pushes `--since`/`--until` bounds into the server-side filter
(timestamp clauses), so it downloads only the window you asked for. Without
time bounds the query streams oldest-first — bound it with `--since` (e.g. an
ISO timestamp 7 days back) if you only need recent entries.

bq mode runs `bq query --format=csv` and the result rows flow through the
same pipeline (query/range/trace/summary/stats) — pull trace data with SQL
and trim it in one step. Rows are decoded as CSV (quoted fields with
embedded commas/newlines keep a single row) and the header row is skipped.

bq tips:

```bash
# TIMESTAMP columns render as "2026-10-13 12:00:00 UTC" — that leaves a
# stray ` UTC` in the output and --since/--until can't read it. Emit ISO
# so cardio understands it:
cardio --bq 'SELECT text, FORMAT_TIMESTAMP("%Y-%m-%dT%H:%M:%SZ", ts) AS timestamp FROM prod.traces'
```

- **The biggest lever is the SQL, not cardio**: SELECT exactly the fields
  you need (`jsonPayload.message`) — never `TO_JSON_STRING(payload)`
  wholesale. A wide SELECT costs more than any trim saves. cardio's edge
  is repetitive or broad downloads; a narrow trace-sized query is already
  optimal without it.
- `--max_rows=10000000` is passed because bq silently caps selects at
  1000 rows otherwise.
- Column names print once on stderr (`Note: bq columns: a | b | …`);
  rows join their fields with a tab, so empty/null columns keep their
  separator and fields stay attributable.
- `--since`/`--until` are NOT pushed into bq (the SQL is yours): filter
  dates in the query with a `WHERE` clause over `FORMAT_TIMESTAMP(...)`
  text, and the same ISO bounds still apply locally.
- `--max-lines` keeps the last N rows (the whole bq output is read
  first — the cap no longer trims the download).

## Flags

| Flag | Effect |
|---|---|
| `[file]` | Log file. Omitted → stdin. |
| `--tail N` | Only the LAST N kept lines (after dedup/query; reads the whole input, ignores the cap). |
| `--raw` | Skip the default agent pipeline: verbatim lines — no dedup, no cap, no timestamp/boilerplate trim. |
| `--project ID` | bq-only: GCP project that runs the job (default = the CLI's configured project). |
| `--max-bytes-billed N` | bq-only: hard bytes-billed cap — the query errors instead of scanning multi-TB tables. |
| `--dry-run` | bq-only: cost estimate only — runs `bq query --dry_run`, no rows read. |
| `--bq-args "--flag=..."` | bq-only: raw bq CLI flags passthrough (whitespace-split, no quotes inside). |
| `--timeout SECS` | bq/gcloud only: kill a hung fetcher after SECS (its job id stays in stderr). |
| `--top N` | Trace mode (full or summary): keep only the biggest N sessions — the rest drop with a note. |
| `--max-field-len N` | cut any field (or line) longer than N chars with a "…" marker. |
| `--agent-md` | Print an agent-facing markdown card (pipe: `cardio --agent-md > AGENTS.md`). |
| `--query TEXT` | Keep only lines containing TEXT (substring, applied first). |
| `--dedup` | Drop repeated whole lines (Bloom filter — **~1% false positives, and they're unique lines thrown away**). |
| `--max-lines N` | Keep only the LAST N lines (recent beat old, whole input reads; events in `--trace`/`--summary` mode). |
| `--json` | NDJSON output: `{"index","content"}` per line; with `--trace` → `{"id","events"}`; with `--summary` → `{"id","count","first","last"}`. |
| `--since TS` | Timestamp >= this. ISO (`2026-10-09T05:00:00`) or `YYYY-MM-DD` (= day start). |
| `--until TS` | Timestamp < this. ISO, or `YYYY-MM-DD` (= NEXT-day start, so the end day is kept). |
| `--trace REGEX` | Group lines by captured ID, e.g. `"request_id=([a-z0-9-]+)"`; sessions printed at the end. |
| `--summary` | With `--trace`: compact per-ID map — event count + first/last event, IDs preserved. |
| `--bq SQL` | Run the bq CLI with this SQL (CSV) and process its rows. One source only. |
| `--stats` | Survey mode: top-20 pattern frequencies to stderr, no stream. Digit runs fold to `#` first (`sid=123` ≡ `sid=456`); `--stats --json` → NDJSON {"pattern","count"} on stdout. |
| `--config FILE` | TOML: `timestamp_regex`, `boilerplate_regex`, `default_trace`. |

## Performance

`./bench.sh` builds the release binary and times every mode on a synthetic
1M-line log (50% duplicate lines, ids, huge XML bodies) — hyperfine when
installed, a monotonic fallback otherwise. First run (M1, release):

| mode | time per 1M lines |
|---|---|
| `cat` baseline | 0.02 s |
| `--query` | 0.12 s |
| `--raw` (no pipeline) | 0.20 s |
| lean (default: trim + dedup + cap) | 0.51 s |
| `--trace` (raw, uncapped) | 0.38 s |
| `--stats` | 0.65 s |

≈ 65 MB/s lean end to end; the trim runs as one combined regex pass. For
gcloud/bq sources the network dominates anyway. (The bench's trace mode
runs `--raw`: in lean mode the session cap bounds input early, so its
time would measure the cap, not the grouper.)

## Time handling

ISO timestamps compare lexicographically (string order == time order). Timezone
offsets are matched and trimmed but comparisons are tz-naive: mixing
`+02:00`-suffixed logs with UTC bounds can misjudge hours around day boundaries.

## Config example

```toml
timestamp_regex = '\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}'
boilerplate_regex = '(?i)\b(INFO|DEBUG|TRACE)\b'
default_trace = 'trace_id=([a-zA-Z0-9-]+)'
```

## Examples

```bash
# agent-ready error digest
cardio app.log --query error --json | agent

# what happened to one request
cardio app.log --trace 'req_id=([a-z0-9-]+)'

# same, compact: first/last event + count per request
cardio app.log --trace 'req_id=([a-z0-9-]+)' --summary

# survey a 10GB log without streaming output
cardio huge.log --stats
```