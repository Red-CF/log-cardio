/// `--agent-md`: the context card. Honest contract only — the card is
/// static text reviewed with the flags; update alongside behavior
/// changes.
pub(crate) static AGENT_MD: &str = r#"# cardio — log trimming for agents

Trim noise from logs so they are cheap to read as tokens. Streams line by
line with near-constant memory (Bloom-filter dedup, capped maps): GB-sized
inputs are fine, they never load into RAM. It can also fetch the logs
itself (`--gcloud`, `--bq`) — the log never touches disk.

## You are an agent: run this

```bash
cardio --lean app.log              # file
tail -f app.log | cardio --lean    # stream
cardio --lean --gcloud 'resource.type=gce_instance'
cardio --lean --bq 'SELECT text AS log FROM prod.traces'
```

**--lean** trims timestamps + boilerplate tokens, drops repeated whole
lines, and caps output at 2000 lines (override the cap with `--max-lines
N` if you need more). Start there; add flags only when the question
demands it.

## Flags (on top of lean)

| Flag | Effect |
|---|---|
| `--query error` | keep only lines containing TEXT (substring) |
| `--max-lines N` | cap output lines (in lean already 2000) |
| `--json` | NDJSON output: {"index","content"} |
| `--since TS` / `--until TS` | keep lines with timestamp >= TS / < TS |
| `--trace REGEX` | group lines by captured ID (e.g. request_id=...), sessions at end |
| `--summary` | with --trace: compact per-ID map (count + first/last) |
| `--stats` | survey mode: top-pattern frequencies, no stream output |
| `--stats --json` | same survey as NDJSON on stdout: {"pattern","count"} |
| `--gcloud FILTER` | stream Cloud Logging (--since/--until pushed server-side) |
| `--bq "SQL"` | stream BigQuery rows via the bq CLI (CSV) |
| `--config FILE` | custom timestamp/boilerplate regexes, default_trace (TOML) |

Without `--lean` you get the raw pipeline (no dedup, no cap) — use it when
you must not lose a single line.

**⚠ Dedup drops unique lines (~1% false positives).** The Bloom filter
cannot tell a repeated line from a hash collision, so roughly 1 in 100
unique lines is discarded as if it were a duplicate. That is a correct
trade for trimming noise, and data loss when you're hunting one weird
event. Never `--lean`/`--dedup` over a log where any line might be THE
answer — run without them (or with `--query` narrowed) instead.

## Output shape
Default: one line per kept event, boilerplate trimmed away.
`--json`: {"index": <n>, "content": "..."}. `--trace` sessions: the ID
and its event lines; `--summary`: {"id","count","first","last"}.

## Timestamps — what bounds accept
Full ISO only: `YYYY-MM-DD` alone, or with time up to `HH:MM[:SS[.frac]]`,
optional `Z` / `±HH[MM][:MM]` offset. `--since 2026-10-01` = day start;
`--until 2026-10-01` = NEXT day start (the end day is kept, so date-to-date
spans behave like `>=` / `<=` on whole days). Invalid bounds exit with an
error instead of streaming unfiltered. Line timestamps are matched inside
the line itself (the line does not have to start with one); compares are
tz-naive — don't mix `+02:00` logs against UTC bounds.

## Gotchas
- A line whose timestamp doesn't match the (configurable) timestamp
  grammar — e.g. a bare date with no time — bypasses the range filter
  and is printed as-is; there is no warning for it today.
- `--gcloud` mode: entries with NO payload (neither text nor json message)
  can't be carried by the current fetch format and are counted, not printed.
- Time ranges belong in the Google query itself when possible
  (`--gcloud` pushes `--since/--until` into its filter; `--bq` does not —
  filter in the SQL `WHERE` over the timestamp column).
"#;