# Changelog

## 2.0.0 (this PR: a slow fetch no longer freezes the server)

- CONCURRENT MCP CORE: one read thread (pings answered while a fetch
  runs), one worker per tools/call, one writer owning stdout. The 1.x
  core ran one request at a time: a slow fetch froze the whole server,
  pings went unanswered, agents watched the wait grow toward infinity.
- Real cancellation: `notifications/cancelled` kills the fetch child's
  process group and answers with an honest `isError`.
- Client-gone awareness: a dead client stdout kills running fetches
  instead of finishing 20-minute scans for nobody.
- fetch budget: heavy fetches capped at 2; further calls queue with
  visible progress. Local file tools are exempt.
- Progress notifications with 10 s heartbeats when the client passes a
  progress token; both pipes drained concurrently (the sequential
  capture deadlocked a real bq run).
- Tool surface: `trim_logs` renamed to `read_log`; NEW `census_log`
  (--group-col census); NEW `fetch_command` (ANY provider CLI piped
  through cardio: the provider-neutral fetch).
- Relicense to Apache-2.0. Version gate re-scoped for infra PRs.

## 1.1.0

- MCP deadlock fix (both pipes drained concurrently) + progress
  notifications; capture caps (8 MiB stdout / 64 KiB stderr).
- `--dedup-fold`: census-level dedup, digits fold to `#` like --stats.
- `CARDIO_MAX_BYTES` env: a default bytes-billed guard for bq.
- `--group-col day,status`: row census per column value.
- Coverage: MCP layer excluded from the ratio (subprocess attribution),
  the rest sits at ~89%.

## 1.0.0

- MCP stdio server (`--mcp`), hand-rolled line-JSON-RPC.
- `--k8s SELECTOR`: kubectl logs with `--since-time` pushdown.
- Adaptive dedup: exact for the first 5000 unique lines, then bloom.
- Window semantics by source: fetchers keep the head, streams keep the
  tail. New `--head`/`--skip`; `--max-lines` removed.
- Lean field cut at 1000 chars by default.
- Windows fetcher tests + `.cmd/.bat` shim resolution.

## 0.2.0

- Module restructure (timestamps/source/pipeline/sessions).
- `--raw` is verbatim (no trim); bq cost kit (`--project`,
  `--max-bytes-billed`, `--dry-run`, `--bq-args`); CSV transport via tab
  join with header note.
- Window-by-tail semantics with honest stderr witnesses.

## 0.1.x

- Initial release: lean pipeline (trim + dedup + window), trace
  grouping, statistics, time-range filtering, gcloud/bq streaming, the
  agent card, CI with coverage badge and releases.
