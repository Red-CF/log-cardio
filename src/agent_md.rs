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
cardio app.log              # file — lean by default
tail -f app.log | cardio    # stream
cardio --gcloud 'resource.type=gce_instance'
cardio --bq 'SELECT text AS log FROM prod.traces'
```

**Default pipeline is lean**: trim + boilerplate stripped + dedup on +
output keeps the LAST 2000 lines — the most recent (`--max-lines N`
overrides). No flags needed — this is the mode that keeps your context
spend low.

**⚠ Default mode drops lines.** Two ways, both deliberate: duplicated
whole lines are removed (Bloom filter, **~1% false positives — a UNIQUE
line can be discarded as if repeated**), and only the last 2000 lines
survive — the HEAD of the range is trimmed away. Correct trade for
trimming noise, data loss when you're hunting one weird event: cardio
never hides it — stderr says how many were dropped and that the cap
kept only the tail. When any line might be THE answer, or the error is
old: `--raw` (nothing dropped, nothing capped, NOTHING trimmed —
timestamps and levels stay), or narrow with `--query`
to read everything you kept.

## Flags

| Flag | Effect |
|---|---|
| `--tail N` | only the LAST N kept lines — the #1 read: "what does the log's latest say" |
| `--raw` | skip the agent pipeline: verbatim — no dedup, no cap, NO trim (timestamps/levels survive) |
| `--query error` | keep only lines containing TEXT (substring) |
| `--max-lines N` | keep the LAST N lines — same tail semantics as --tail |
| `--json` | NDJSON output: {"index","content"} |
| `--since TS` / `--until TS` | keep lines with timestamp >= TS / < TS |
| `--trace REGEX` | group lines by captured ID (e.g. request_id=...), sessions at end |
| `--summary` | with --trace: compact per-ID map (count + first/last; `--top N` when IDs are many) |
| `--stats` | survey mode: top-pattern frequencies, no stream output (digit runs fold to # — per-id noise groups) |
| `--stats --json` | same survey as NDJSON on stdout: {"pattern","count"} |
| `--gcloud FILTER` | stream Cloud Logging (--since/--until pushed server-side) |
| `--bq "SQL"` | stream BigQuery rows via the bq CLI (CSV) |
| `--dry-run` | bq only: cost estimate (`bq query --dry_run`), no rows read |
| `--max-bytes-billed N` | bq only: errors out if the job would bill more than N bytes |
| `--project ID` | bq only: project that runs the job (default = CLI's config) |
| `--bq-args "…"` | bq only: raw bq CLI flags passthrough (whitespace-split) |
| `--timeout SECS` | bq/gcloud only: kill a hung fetcher (its job id stays in stderr) |
| `--top N` | trace mode (full or summary): keep only the biggest N sessions — the rest drop with a note |
| `--max-field-len N` | cut huge fields/lines (XML bodies) with a "…" marker |
| `--config FILE` | custom timestamp/boilerplate regexes, default_trace (TOML) |

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
- `--bq` mode: column names print once to stderr ("Note: bq columns: …");
  row fields join with TAB, so empty/falsey columns keep separator. bq
  status chatter is suppressed. Cost guards: `--dry-run` estimates,
  `--max-bytes-billed` caps, `--project` selects the exec project.
- `--gcloud` mode: entries with NO payload (neither text nor json message)
  can't be carried by the current fetch format and are counted, not printed.
  Without `--since` it streams the whole matching range (a warning fires
  before the fetch).
- Time ranges belong in the Google query itself when possible
  (`--gcloud` pushes `--since/--until` into its filter; `--bq` does not —
  filter in the SQL `WHERE` over the timestamp column).
"#;