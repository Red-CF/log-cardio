# cardio

Fast Rust CLI that trims the fat from logs — fewer tokens for LLM agents, faster
search and manipulation for developers. Logs can be GBs: cardio streams line by
line with near-constant memory (Bloom-filter dedup, capped stats/trace maps).

## Install

```bash
cargo install --path .
```

## Input

```bash
cardio app.log                      # file
tail -f app.log | cardio            # stdin
cardio --gcloud 'resource.type=gce_instance'   # fetch straight from Cloud Logging
cardio --bq 'SELECT text, timestamp FROM prod.traces'   # fetch via BigQuery SQL
```

gcloud mode pushes `--since`/`--until` bounds into the server-side filter
(timestamp clauses), so it downloads only the window you asked for. Without
time bounds the query streams oldest-first — bound it with `--since` (e.g. an
ISO timestamp 7 days back) if you only need recent entries.

bq mode runs `bq query --format=csv` and every result row flows through the
same pipeline (query/range/trace/summary/stats) — pull trace data with SQL
and trim it in one step. bq's row limits still apply (cardio passes
`--max_rows=10000000`).

## Flags

| Flag | Effect |
|---|---|
| `[file]` | Log file. Omitted → stdin. |
| `--query TEXT` | Keep only lines containing TEXT (substring, applied first). |
| `--dedup` | Drop repeated whole lines (Bloom filter, ~1% false positives, memory constant). |
| `--max-lines N` | Cap output (events in `--trace`/`--summary` mode). |
| `--json` | NDJSON output: `{"index","content"}` per line; with `--trace` → `{"id","events"}`; with `--summary` → `{"id","count","first","last"}`. |
| `--since TS` | Timestamp >= this. ISO (`2026-10-09T05:00:00`) or `YYYY-MM-DD` (= day start). |
| `--until TS` | Timestamp < this. ISO, or `YYYY-MM-DD` (= NEXT-day start, so the end day is kept). |
| `--trace REGEX` | Group lines by captured ID, e.g. `"request_id=([a-z0-9-]+)"`; sessions printed at the end. |
| `--summary` | With `--trace`: compact per-ID map — event count + first/last event, IDs preserved. |
| `--bq SQL` | Run the bq CLI with this SQL (CSV) and process its rows. One source only. |
| `--stats` | Survey mode: top-20 pattern frequencies to stderr, no stream. |
| `--config FILE` | TOML: `timestamp_regex`, `boilerplate_regex`, `default_trace`. |

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