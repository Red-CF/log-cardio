# log-cardio 🏃🏼‍♂️

[![CI](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml/badge.svg)](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml)
[![coverage](https://raw.githubusercontent.com/Red-CF/log-cardio/gh-pages/coverage.svg)](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/log-cardio.svg)](https://crates.io/crates/log-cardio)
[![macOS ARM64](https://img.shields.io/badge/macOS-ARM64-999999.svg)](https://github.com/Red-CF/log-cardio/releases)
[![macOS Intel](https://img.shields.io/badge/macOS-Intel-999999.svg)](https://github.com/Red-CF/log-cardio/releases)
[![Linux](https://img.shields.io/badge/Linux-X64-fcc624.svg)](https://github.com/Red-CF/log-cardio/releases)
[![Windows](https://img.shields.io/badge/Windows-X64-0078d6.svg)](https://github.com/Red-CF/log-cardio/releases)

`cardio` is a fast Rust CLI that trims the fat from logs so LLM agents
spend fewer tokens reading them. Logs can be GBs and cardio streams them
line by line with near-constant memory (Bloom-filter dedup, capped
stats and trace maps). Agents can also fetch the logs themselves
(`--gcloud`, `--bq`) and learn the whole tool from one card
(`cardio --agent-md`).

The project is currently a WIP: it is taking its first steps and the
test outcomes are all green so far. A thorough benchmark pass is planned
(see [Performance](#performance) for the first numbers).

## Install

With cargo (installs the `cardio` binary):

```bash
cargo install log-cardio
```

Without cargo (macOS and Linux; downloads the binary from the latest
release):

```bash
curl -fsSL https://raw.githubusercontent.com/Red-CF/log-cardio/main/install.sh | sh
```

Windows: grab the `cardio-Windows-X64.exe` from the
[releases page](https://github.com/Red-CF/log-cardio/releases) and put
it on your `PATH`.

Or build from source:

```bash
cargo install --path .
```

## First run: getting oriented

Cardio explains itself. Before reading any log, an agent (or person)
should do these three things:

```bash
cardio --agent-md > AGENTS.md   # read the contract first: flags, output shapes, gotchas
cardio app.log --stats          # survey: what does this log even contain?
cardio app.log                  # then the lean read of what matters
```

Writing `AGENTS.md` to your repo root once means agent runtimes pick it
up automatically. Agents never have to re-learn the tool by trial and
error, which is exactly the token cost this tool exists to save.

## Input

```bash
cardio app.log                 # file: lean by default (trim + dedup + cap of 2000)
cardio --gcloud 'resource.type=gce_instance'
cardio --bq 'SELECT FORMAT_TIMESTAMP("%Y-%m-%dT%H:%M:%SZ", ts) AS timestamp, text FROM prod.traces'
cardio --raw app.log           # fidelity: no dedup, no cap, no trim at all
```

> **⚠ The default pipeline drops lines.** Two deliberate ways, both
> documented here so you can trust what you read:
>
> 1. Repeated whole lines are removed. This uses a Bloom filter, so
>    roughly 1% of dropped lines may actually be unique. cardio prints
>    how many were dropped on stderr.
> 2. Only the last 2000 lines are kept. The most recent lines win, and
>    the head of the range is trimmed away. `--max-lines` overrides it.
>
> This is the right trade for noisy logs. When any line might be THE
> answer, run `--raw`, or narrow with `--query` and read everything you
> kept.

### gcloud mode

`--since` and `--until` are pushed into the server-side filter as
timestamp clauses, so the download only covers the window you asked
for. Without time bounds the query streams the oldest entries first,
which can be very expensive. A warning tells you before that happens,
but the fix is yours: add `--since` (for example an ISO timestamp 7
days back) when you only need recent entries.

### bq mode

bq mode runs `bq query --format=csv` and feeds the resulting rows
through the same pipeline (query, range, trace, summary, stats). You
pull trace data with SQL and trim it in one step. Rows are decoded as
CSV, so quoted fields with embedded commas or newlines stay a single
row, and the header row is skipped.

One thing to know: TIMESTAMP columns render in bq as
`2026-10-13 12:00:00 UTC`, and `--since`/`--until` cannot parse that.
Ask bq to emit ISO instead:

```bash
cardio --bq 'SELECT text, FORMAT_TIMESTAMP("%Y-%m-%dT%H:%M:%SZ", ts) AS timestamp FROM prod.traces'
```

bq tips:

- **The biggest lever is the SQL, not cardio.** SELECT exactly the
  fields you need (for example `jsonPayload.message`), and never pull
  whole payloads with `TO_JSON_STRING(payload)`. A wide SELECT costs
  more than any trim will save. cardio shines when the download is
  repetitive or broad; a narrow, trace-sized query is already optimal
  without it.
- `--max_rows=10000000` is passed to bq because bq silently caps
  selects at 1000 rows otherwise.
- Column names print once on stderr (`Note: bq columns: a | b | ...`),
  so you always know which value is which. Rows join their fields with
  a tab, and empty or null columns keep their separator instead of
  vanishing.
- `--since`/`--until` are NOT pushed into bq, because the SQL is yours.
  Filter dates in the query with a `WHERE` clause over
  `FORMAT_TIMESTAMP(...)` text, and the same ISO bounds still apply
  locally.
- `--max-lines` keeps the last N rows. The whole bq output is read
  first: the cap no longer trims the download itself.

## Flags

| Flag | Effect |
|---|---|
| `[file]` | Log file. If omitted, reads from stdin. |
| `--query TEXT` | Keep only lines containing TEXT (substring, applied first). |
| `--since TS` | Timestamp >= this. Accepts ISO (`2026-10-09T05:00:00`) or `YYYY-MM-DD` (day start). |
| `--until TS` | Timestamp < this. ISO, or `YYYY-MM-DD` (NEXT-day start, so the end day is kept). |
| `--raw` | Skip the default pipeline: verbatim lines, no trim of timestamps or boilerplate, no dedup, no cap. |
| `--tail N` | Only the LAST N kept lines (after dedup and query). Reads the whole input, replaces the cap. |
| `--max-lines N` | Same tail semantics as `--tail`; in `--trace`/`--summary` mode it caps events per run instead. |
| `--max-field-len N` | Cut any field (or whole line) longer than N chars and mark the cut with `...`. |
| `--json` | NDJSON output: `{"index","content"}` per line; with `--trace`, `{"id","events"}`; with `--summary`, `{"id","count","first","last"}`. |
| `--trace REGEX` | Group lines by captured ID, for example `"request_id=([a-z0-9-]+)"`. Sessions print at the end. |
| `--summary` | With `--trace`: compact per-ID map with event count plus first and last events. IDs preserved. |
| `--top N` | In trace mode (full or summary): keep only the biggest N sessions; the rest drop with a note. |
| `--stats` | Survey mode: top 20 pattern frequencies on stderr, no stream output. Digit runs fold to `#` first, so `sid=123` and `sid=456` count as one pattern. `--stats --json` prints NDJSON `{"pattern","count"}` on stdout. |
| `--dedup` | Force dedup in `--raw` mode. Dropped count reported on stderr. Default pipeline already has dedup on. |
| `--gcloud FILTER` | Stream Cloud Logging entries matching this filter. `--since`/`--until` are pushed server-side. |
| `--bq SQL` | Run the bq CLI with this SQL and process its rows. Exactly one source at a time. |
| `--project ID` | bq only: the GCP project that runs the job. Defaults to the CLI's configured project. |
| `--max-bytes-billed N` | bq only: hard cap on billed bytes. The query errors instead of scanning multi-TB tables. |
| `--dry-run` | bq only: cost estimate only. Runs `bq query --dry_run` and reads no rows. |
| `--bq-args "--flag=..."` | bq only: pass extra raw bq CLI flags (split on whitespace, no quotes inside). |
| `--timeout SECS` | bq or gcloud: kill a hung fetcher after SECS. Its job id stays visible on stderr. |
| `--config FILE` | TOML file with `timestamp_regex`, `boilerplate_regex`, `default_trace`. |
| `--agent-md` | Print the agent-facing markdown card (pipe it: `cardio --agent-md > AGENTS.md`). |

## Performance

`./bench.sh` builds the release binary and times every mode on a
synthetic 1M-line log (50% duplicate lines, digit ids, huge XML
bodies). It uses hyperfine when installed and falls back to a plain
monotonic-clock loop. First run (M1, release build):

| mode | time per 1M lines |
|---|---|
| `cat` baseline | 0.02 s |
| `--query` | 0.12 s |
| `--raw` (no pipeline) | 0.20 s |
| `--trace` (raw, uncapped) | 0.38 s |
| lean (default: trim + dedup + cap) | 0.51 s |
| `--stats` | 0.65 s |

That is roughly 65 MB/s on the lean path end to end. The trim runs as a
single combined regex pass. When the input comes from gcloud or bq, the
network dominates anyway. The bench's trace scenario runs with `--raw`
on purpose: in lean mode the session cap bounds the input early, so its
time would measure the cap rather than the grouper.

## Tests

```bash
cargo test    # 61 total: 55 CLI integration (tests/cli.rs) + unit (src/timestamps.rs)
```

Philosophy: black-box, at the CLI level. Tests pin the promises the
agent card makes (trim behavior, dedup honesty, exit codes, stderr
witnesses, JSON shapes). Because of that they survive refactors, and
any drift between docs and behavior fails loudly. gcloud and bq are
faked with PATH-stub shell scripts (marked `#[cfg(unix)]`), so the
whole suite runs offline on every OS. Unit tests exist only for pure
math that a CLI test cannot probe sanely, such as calendar rollovers
and the 9999-12-31 clamp.

## Time handling

ISO timestamps compare lexicographically, so string order equals time
order. Timezone offsets are matched and trimmed for comparison, but the
comparison is tz-naive: mixing `+02:00`-suffixed logs with UTC bounds
can misjudge hours around day boundaries.

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