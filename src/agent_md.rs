/// `--agent-md`: the context card. Honest contract only — the card is
/// static text reviewed with the flags; update alongside behavior
/// changes.
pub(crate) static AGENT_MD: &str = r#"# cardio — agent context

Rust CLI that trims noise from logs so they are cheap to read as tokens.
Streams line by line with near-constant memory (Bloom-filter dedup, capped
maps): GB-sized inputs are fine, they never load into RAM. It can also fetch
logs itself via `--gcloud` / `--bq`.

## Why use it
Logs are full of boilerplate lines that repeat across every entry. cardio
strips timestamps and boilerplate tokens, filters by range, and can group
events by ID — one pass, deterministic, streaming.

## Cheatsheet
| Command | Effect |
|---|---|
| `cardio app.log` | trim a file (stdin if omitted: `... \| cardio`) |
| `--query error` | keep only lines containing TEXT (substring) |
| `--dedup` | drop whole repeated lines (Bloom filter, ~1% false positives) |
| `--max-lines 100` | cap output lines |
| `--json` | NDJSON output: {"index","content"} |
| `--since TS` | keep lines with timestamp >= TS |
| `--until TS` | keep lines with timestamp < TS |
| `--trace REGEX` | group lines by the captured ID, sessions at the end |
| `--summary` | with --trace: compact per-ID map (count + first/last) |
| `--stats` | survey to stderr (top patterns), no stream output |
| `--gcloud FILTER` | stream Cloud Logging (since/until pushed to the server) |
| `--bq "SQL"` | stream BigQuery rows via the bq CLI (CSV) |
| `--config FILE`  | custom timestamp/boilerplate regexes, default_trace (TOML) |

## Output shape
Default: one line per kept event, `content` with boilerplate trimmed away.
`--json`: {"index": <n>, "content": "..."}. `--trace` sessions: the ID and
its event lines; `--summary`: {"id","count","first","last"}.

## Timestamps — what bounds accept
Full ISO only: `YYYY-MM-DD` alone, or with time up to `HH:MM[:SS[.frac]]`,
optional `Z` / `±HH[MM][:MM]` offset. `--since 2026-10-01` = day start;
`--until 2026-10-01` = NEXT day start (the end day is kept, so date-to-date
spans behave like `>=` / `<=` on whole days). Invalid bounds exit with an
error instead of streaming unfiltered. Line timestamps are matched inside
the line itself (the line does not have to start with one); compares are
tz-naive — don't mix `+02:00` logs against UTC bounds.

## Gotchas
- 25 lines of "INFO starting worker" cost 25 output lines minus dedup:
  `--dedup` is the single biggest token saver on repetitive files.
- A line whose timestamp doesn't match the (configurable) timestamp
  grammar — e.g. a bare date with no time — bypasses the range filter
  and is printed as-is; there is no warning for it today.
- `--gcloud` mode: entries with NO payload (neither text nor json message)
  can't be carried by the current fetch format and are counted, not printed.
"#;
