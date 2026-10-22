/// `--agent-md`: the context card. Honest contract only. The card is
/// static text reviewed with the flags; update it alongside behavior
/// changes, and the anti-staleness test keeps flag drift failing CI.
pub(crate) static AGENT_MD: &str = r#"# cardio: log trimming for agents

You are reading the card printed by `cardio --agent-md`; re-print it
whenever the binary updates so your context stays current.

Trim noise from logs so they are cheap to read as tokens. Streams line by
line with near-constant memory, so GB-sized inputs are fine. It can also
fetch the logs itself (`--gcloud`, `--bq`, `--k8s`), and the log never
touches disk. Works with ANY log source actually: the trim engine reads
stdin, so `kubectl logs -f x | cardio`, `aws logs tail ... | cardio`,
`journalctl | cardio` all work with no provider flags.

## You are an agent: run this

```bash
cardio app.log              # file, lean by default
tail -f app.log | cardio    # stream
cardio --gcloud 'resource.type=gce_instance' --since 2026-10-09
cardio --bq 'SELECT text AS log FROM prod.traces'
cardio --k8s deployment/my-api --since 2026-10-09   # one pod's logs
```

Many questions over the same window? Download once, then iterate free:

```bash
cardio --bq 'SQL' > cache.txt     # one download, trimmed
cardio cache.txt --query NTS     # re-queries cost no fetch at all
```

The default pipeline is lean: trim + boilerplate stripped + dedup on +
a window of 2000 kept lines. The window's side follows the source:
cloud fetchers keep the FIRST 2000 (your query's ordering is your
intent), files and streams keep the LAST 2000 (recent beats old).
Explicit `--head N` / `--tail N` override. No flags needed: this is the
mode that keeps your context spend low.

**⚠ Default mode drops lines.** Three deliberate ways, all announced on
stderr, all worth knowing:

1. Duplicated whole lines are removed. Under 5000 unique lines the dedup
   is EXACT (zero false positives); above that it switches to a bloom
   filter where a few dropped lines may actually be unique (the switch
   says so). Note: lines that differ only in trimmed boilerplate (or
   timestamp) collapse into one by design — for control-flow-style
   streams where every event differs, prefer `--trace` (no dedup) or
   `--raw`.
2. Only 2000 lines survive; the side follows the source (fetchers = head,
   streams = tail). A fetcher's stream is STOPPED at the cap and killed
   (partial result delivered, the rest not read).
3. Fields longer than 1000 characters are cut with a "…" marker (lean
   only; JWTs and payload bodies otherwise eat the context). 0 disables.

When any line might be THE answer: run `--raw` (nothing trimmed, deduped,
capped or cut) or narrow with `--query` to read everything you kept.

## Parser gotcha

A flag that TAKES A VALUE must have its value next to it:
`cardio --bq "SQL" --dry-run` works; `cardio --bq --dry-run "SQL"` fails
with "a value is required for '--bq'" (the second flag is where the value
would sit). Pass value-flags first when unsure.

## Flags

| Flag | Effect |
|---|---|
| `--tail N` | keep only the LAST N kept lines — the most recent beat the old; the #1 read when you want what the log just said |
| `--head N` | keep only the FIRST N kept lines — the lean default for cloud fetchers; in --trace/--summary mode this is the event cap (earliest events win) |
| `--skip N` | drop the first N kept lines before the window; bq pages as `--skip 100 --head 100` |
| `--raw` | skip the agent pipeline: verbatim. No dedup, no cap, no trim, no field cut (timestamps and levels survive) |
| `--query error` | keep only lines containing TEXT (substring) |
| `--json` | NDJSON output: {"index","content"} |
| `--since TS` / `--until TS` | keep lines with timestamp >= TS / < TS |
| `--trace REGEX` | group lines by captured ID (e.g. request_id=...), sessions listed at the end |
| `--summary` | with --trace: compact per-ID map (count + first/last). Use `--top N` when IDs are many |
| `--top N` | trace mode (full or summary): keep only the biggest N sessions, the rest drop with a note |
| `--max-field-len N` | cut huge fields or lines (XML/JWT bodies) after N chars with a "…" marker (lean default 1000; 0 = unlimited) |
| `--stats` | survey mode: top-pattern frequencies, no stream output. Digit runs fold to #, so per-id noise groups |
| `--dedup-fold` | a dedup level for censuses: lines differing only in digit runs collapse (folds digits to # like --stats); the run notes say it |
| `--group-col day,status` | bq census: count rows per column value, printed like --stats (NDJSON with --json). Rows go verbatim; only with --bq |
| `--stats --json` | same survey as NDJSON on stdout: {"pattern","count"} |
| `--dedup` | with --raw: drop repeated lines after all (lean mode has dedup on by default) |
| `--gcloud FILTER` | stream Cloud Logging (--since/--until pushed server-side) |
| `--bq "SQL"` | stream BigQuery rows via the bq CLI (CSV) |
| `--k8s SELECTOR` | stream `kubectl logs` for a pod/deployment/bare name; --since pushes to --since-time |
| `-c, --container NAME` | with --k8s: the container of the pod (kubectl -c) |
| `--dry-run` | bq only: cost estimate (`bq query --dry_run`), no rows read |
| `--max-bytes-billed N` | bq only: errors out if the job would bill more than N bytes |
| `--project ID` | bq only: project that runs the job (default: the CLI's configured project) |
| `--bq-args "..."` | bq only: raw bq CLI flags passthrough (split on whitespace, no quotes inside) |
| `--timeout SECS` | kill a hung fetcher (its job id stays in stderr). DEFAULT 600 s on STREAMING fetchers (gcloud, k8s) — a stopped flow IS the hang. bq waits whole jobs: it needs an EXPLICIT --timeout (0 = disable) |
| `--config FILE` | custom timestamp/boilerplate regexes and default_trace (TOML) |
| `--mcp` | run as an MCP stdio server exposing these modes as tools (see cardio's README) |

## Output shape
Default: one line per kept event, boilerplate trimmed away. `--json`:
{"index": <n>, "content": "..."}. `--trace` sessions: the ID and its
event lines; `--summary`: {"id","count","first","last"}.

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
- `--bq` mode: a cost guard can live in the ENV (CARDIO_MAX_BYTES=100GB)
  so wide scans error instead of running wide on autopilot; 0 disables,
  the explicit --max-bytes-billed flag always wins. Column names print
  once to stderr ("Note: bq columns: …");
  row fields join with a TAB, so empty columns keep their separator.
  bq status chatter is suppressed. Cost guards: `--dry-run` estimates,
  `--max-bytes-billed` caps, `--project` selects the exec project.
- `--gcloud` mode: entries with NO payload (neither text nor json message)
  cannot be carried by the current fetch format and are counted, not
  printed. Without `--since`, it streams the whole matching range; a
  warning fires before the fetch.
- `--k8s` mode: namespace and context are kubectl's own (current ones);
  kubectl runs with --timestamps so the local --until actually binds
  (ts-less output would bypass the range filter entirely).
- GKE pods: Cloud Logging has them as resource.type=k8s_container — the
  --gcloud path covers GKE clusters with full pushdown; --k8s talks to
  kubectl for plain clusters or local pods.
- Time ranges belong in the Google query itself when possible
  (`--gcloud` pushes `--since/--until` into its filter; `--bq` does not,
  so filter in the SQL `WHERE` over the timestamp column).
"#;