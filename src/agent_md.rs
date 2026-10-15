/// `--agent-md`: the context card. Honest contract only. The card is
/// static text reviewed with the flags; update it alongside behavior
/// changes.
pub(crate) static AGENT_MD: &str = r#"# cardio: log trimming for agents

Trim noise from logs so they are cheap to read as tokens. Streams line by
line with near-constant memory (Bloom-filter dedup, capped maps), so
GB-sized inputs are fine: they never load into RAM. It can also fetch the
logs itself (`--gcloud`, `--bq`), and the log never touches disk.

## You are an agent: run this

```bash
cardio app.log              # file, lean by default
tail -f app.log | cardio    # stream
cardio --gcloud 'resource.type=gce_instance'
cardio --bq 'SELECT text AS log FROM prod.traces'
```

The default pipeline is lean: trim + boilerplate stripped + dedup on +
output keeps the LAST 2000 lines (the most recent; `--max-lines N`
overrides it). No flags needed: this is the mode that keeps your context
spend low.

**⚠ Default mode drops lines.** Two ways, both deliberate:

1. Duplicated whole lines are removed with a Bloom filter. About 1% of
   dropped lines may actually be unique.
2. Only the last 2000 lines survive: the HEAD of the range is trimmed
   away.

This is the right trade when trimming noise, and data loss when you are
hunting one weird event. cardio never hides it: stderr says how many
lines were dropped and that the cap kept only the tail. When any line
might be THE answer, or the error is old, run `--raw` (nothing dropped,
nothing capped, nothing trimmed: timestamps and levels stay) or narrow
with `--query` to read everything you kept.

## Flags

| Flag | Effect |
|---|---|
| `--tail N` | only the LAST N kept lines. The #1 read, when you want what the log just said |
| `--raw` | skip the agent pipeline: verbatim. No dedup, no cap, no trim (timestamps and levels survive) |
| `--dedup` | with --raw: drop repeated lines after all (lean mode has dedup on by default) |
| `--query error` | keep only lines containing TEXT (substring) |
| `--max-lines N` | keep the LAST N lines, same tail semantics as --tail |
| `--json` | NDJSON output: {"index","content"} |
| `--since TS` / `--until TS` | keep lines with timestamp >= TS / < TS |
| `--trace REGEX` | group lines by captured ID (e.g. request_id=...), sessions listed at the end |
| `--summary` | with --trace: compact per-ID map (count + first/last). Use `--top N` when IDs are many |
| `--top N` | trace mode (full or summary): keep only the biggest N sessions, the rest drop with a note |
| `--stats` | survey mode: top-pattern frequencies, no stream output. Digit runs fold to #, so per-id noise groups |
| `--stats --json` | same survey as NDJSON on stdout: {"pattern","count"} |
| `--gcloud FILTER` | stream Cloud Logging (--since/--until pushed server-side) |
| `--bq "SQL"` | stream BigQuery rows via the bq CLI (CSV) |
| `--dry-run` | bq only: cost estimate (`bq query --dry_run`), no rows read |
| `--max-bytes-billed N` | bq only: errors out if the job would bill more than N bytes |
| `--project ID` | bq only: project that runs the job (default: the CLI's configured project) |
| `--bq-args "..."` | bq only: raw bq CLI flags passthrough (split on whitespace, no quotes inside) |
| `--timeout SECS` | bq or gcloud: kill a hung fetcher (its job id stays in stderr) |
| `--max-field-len N` | cut huge fields or lines (XML bodies) after N chars, with a "…" marker |
| `--config FILE` | custom timestamp/boilerplate regexes and default_trace (TOML) |

## Output shape
Default: one line per kept event, boilerplate trimmed away.
`--json`: {"index": <n>, "content": "..."}. `--trace` sessions: the ID
and its event lines; `--summary`: {"id","count","first","last"}.

## Timestamps: what bounds accept
Full ISO only: `YYYY-MM-DD` alone, or with time up to `HH:MM[:SS[.frac]]`,
plus an optional `Z` or `±HH[MM][:MM]` offset. `--since 2026-10-01` means
day start. `--until 2026-10-01` means NEXT day start, so the end day is
kept and date-to-date spans behave like `>=` and `<=` on whole days.
Invalid bounds exit with an error instead of streaming unfiltered. Line
timestamps are matched anywhere inside the line (the line does not have
to start with one). Compares are tz-naive: do not mix `+02:00` logs
against UTC bounds.

## Gotchas
- SQL discipline beats the trimmer. SELECT exactly the fields you need
  (e.g. `jsonPayload.message`) instead of whole payloads
  (`TO_JSON_STRING(jsonPayload)`): a wide SELECT costs more than any
  trim saves, and cardio cannot fix column choice. cardio's edge is
  repetitive or broad raw downloads; when your query is already narrow
  and trace-sized, filtering in SQL is cheaper and cardio adds little.
- A line whose timestamp does not match the (configurable) timestamp
  grammar (e.g. a bare date with no time) bypasses the range filter and
  is printed as-is; there is no warning for it today.
- `--bq` mode: column names print once to stderr ("Note: bq columns: …");
  row fields join with a TAB, so empty columns keep their separator.
  bq status chatter is suppressed. Cost guards: `--dry-run` estimates,
  `--max-bytes-billed` caps, `--project` selects the exec project.
- `--gcloud` mode: entries with NO payload (neither text nor json message)
  cannot be carried by the current fetch format and are counted, not
  printed. Without `--since`, it streams the whole matching range; a
  warning fires before the fetch.
- Time ranges belong in the Google query itself when possible
  (`--gcloud` pushes `--since/--until` into its filter; `--bq` does not,
  so filter in the SQL `WHERE` over the timestamp column).
"#;