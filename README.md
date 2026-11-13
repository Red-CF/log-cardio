# log-cardio 🏃🏼‍♂️

[![CI](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml/badge.svg)](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml)
[![coverage](https://raw.githubusercontent.com/Red-CF/log-cardio/gh-pages/coverage.svg)](https://github.com/Red-CF/log-cardio/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/log-cardio.svg)](https://crates.io/crates/log-cardio)
[![bench](https://raw.githubusercontent.com/Red-CF/log-cardio/gh-pages/bench.svg)](https://htmlpreview.github.io/?https://github.com/Red-CF/log-cardio/blob/gh-pages/bench.html)
[![macOS ARM64](https://img.shields.io/badge/macOS-ARM64-999999.svg)](https://github.com/Red-CF/log-cardio/releases)
[![macOS Intel](https://img.shields.io/badge/macOS-Intel-999999.svg)](https://github.com/Red-CF/log-cardio/releases)
[![Linux](https://img.shields.io/badge/Linux-X64-fcc624.svg)](https://github.com/Red-CF/log-cardio/releases)
[![Windows](https://img.shields.io/badge/Windows-X64-0078d6.svg)](https://github.com/Red-CF/log-cardio/releases)

`cardio` is a fast Rust CLI that trims the fat from logs so LLM agents
spend fewer tokens reading them. Logs can be GBs and cardio streams them
line by line with near-constant memory. Agents can also fetch the logs
themselves — `--gcloud`, `--bq`, or `--k8s` for pod logs — and learn the
whole tool from one card (`cardio --agent-md`).

The project is in early-days mode (2.0.0 just shipped): every test
green, and the honest numbers live in the [Performance](#performance)
section.

## Who is cardio for?

- **You debug with LLM agents** (Claude Code, Codex, Cursor...) and are
  tired of them burning half the context window repeating the same line
  800 times or pasting 4KB request bodies whole. cardio is the log layer
  the agent keeps on the other side: it trims, dedups, censors and
  windows — and its own card teaches the agent the contract so it stops
  guessing.
- **You work on GCP or Kubernetes** and the agent fetches logs itself:
  `--gcloud`, `--bq` and `--k8s` push time bounds SERVER-side, so the
  agent downloads only the window it asks for — with cost guards
  (`--dry-run`, byte caps, env guard) so a first-pass exploratory query
  fails fast instead of scanning 68GB.
- **You run any other source** — Azure, AWS, journalctl, docker logs,
  plain files: cardio's engine reads stdin and its MCP `fetch_command`
  pipes any provider CLI through the same pipeline. Google is a preset,
  not a prerequisite.
- **You care about trust in what the agent reads**: every drop says so
  on stderr — repeats collapsed, what the window kept, what got cut.
  No silent loss, ever.

NOT for: small logs that fit the context anyway (plain shell is free
and always installed), log pipelines that never feed an LLM, or teams
looking for a retention/aggregation product — cardio is a read layer
for agents, not an observability suite.

## Why not just grep?

You can: `tail -n 50000 app.log | grep -v DEBUG` is free and works. The
difference is what reaches the context window, and who tells you when
something got dropped:

| | plain shell | cardio |
|---|---|---|
| repeated lines | flood, or you compose `uniq -c` tricks | collapsed (exact dedup under 5000 unique lines, announced bloom after) |
| what you dropped | nothing tells you | stderr says: how many repeats, what the window kept, what got cut |
| "what happened to THIS request" | compose grep/awk yourself | `--trace 'req_id=(...)'` groups the whole session |
| "what does this log even contain" | read a huge sample | `--stats` top-20 patterns with ids folded |
| big fetches | you carry the download | pushdown: only your window downloads (gcloud/k8s); bq gets cost guards (dry-run, byte caps) |
| timestamps, trace ids | you regex them yourself | the trim knows the grammar; `--raw` hands you everything verbatim |

The one thing plain shell is better at: it is already installed. That is
a fair reason when your log fits; cardio exists for the 10GB cases.

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

## Updating and uninstalling

- **Installed with cargo**: update by re-running `cargo install log-cardio`;
  remove with `cargo uninstall cardio`.
- **Installed with the script**: update by re-running the install script
  (it overwrites the binary with the latest release):

```bash
curl -fsSL https://raw.githubusercontent.com/Red-CF/log-cardio/main/install.sh | sh
```

Remove it:

```bash
curl -fsSL https://raw.githubusercontent.com/Red-CF/log-cardio/main/uninstall.sh | sh
```

cardio installs exactly one file, a static binary (at `/usr/local/bin/cardio`
or `~/.local/bin/cardio`). There is no config, cache, or daemon anywhere:
deleting that file is a complete removal.

- **Windows**: delete the `.exe` you put on your `PATH`.

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

## How cardio works

Every line you give cardio flows through a small pipeline, and every
stage that drops something SAYS so on stderr:

```
              ┌──────────┐   ┌─────────────┐   ┌──────────────┐
 input ─────► │ query    │──►│ time range  │──►│ trim (ts +   │──► dedup exact
 (file/fetch) │ (cheap)  │   │ ISO bounds  │   │ boilerplate) │   ≤5000 unique
              └──────────┘   └─────────────┘   └──────────────┘
                                                    ► window ─► output
                                                      HEAD if fetcher /
                                                      TAIL if stream
 each drop ─► stderr note (dedup count, cap side, field cuts) ── honest
```

WHY a window whose side follows the source: for a cloud fetcher, YOUR
query ordering is your intent (`--bq 'SELECT ... ORDER BY ASC'` means
the head of the result matters); for a live file, the tail is the news.
WHY the 1000-char field cut lives in lean: JWTs and request bodies used
to pass whole on every line. WHY dedup is exact first: two agent runs
distrusted the bloom's 1% false positives — exactness where the trust
lives, probabilistic cost only on huge inputs. The deep machine (the
timestamp invariant, the MCP thread model, the fetcher guards) is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Input

```bash
cardio app.log                  # file: lean by default
cardio --gcloud 'resource.type=gce_instance' --since 2026-10-09
cardio --bq 'SELECT FORMAT_TIMESTAMP("%Y-%m-%dT%H:%M:%SZ", ts) AS timestamp, text FROM prod.traces'
cardio --k8s deployment/my-api --since 2026-10-09     # kubectl logs with pushdown
kubectl logs -f pod/x | cardio  # ANY provider works through plain stdin
cardio --raw app.log            # fidelity: no dedup, no cap, no trim at all
```

> **⚠ The default (lean) pipeline drops lines.** Three deliberate ways,
> each announced on stderr so the read stays trustworthy:
>
> 1. Repeated whole lines are removed — deduplicated EXACTLY for the
>    first 5000 unique lines; then a bloom filter takes over (~1% of its
>    drops may be unique; the switch is announced). Lines that differ
>    only in trimmed boilerplate (or in their timestamp) collapse by
>    design: for that class use `--trace` (keeps every event) or `--raw`.
> 2. A window of 2000 kept lines. The side follows the source: cloud
>    fetchers keep the FIRST 2000 (the query's ordering is your intent);
>    files and streams keep the LAST 2000 (recent beats old). At the cap
>    a fetcher's stream is stopped and its child killed; the note says so.
> 3. Fields longer than 1000 chars are cut with a "…" marker (lean only;
>    `--max-field-len 0` disables, `--raw` never cuts).
>
> This is the right trade for noisy logs. When any line might be THE
> answer, run `--raw`, or narrow with `--query` and read everything you
> kept.

### Fetching from cloud providers

`--gcloud`: `--since`/`--until` are pushed into the server-side filter
as timestamp clauses, so the download only covers the window you asked
for. Without time bounds the query streams the oldest entries first: a
warning tells you before it happens, and the fix is yours (add
`--since`).

`--bq`: runs `bq query --format=csv` (`--quiet` on, rows decoded as CSV,
header skipped) and feeds the rows through the same pipeline. The cost
guards are there for you: `--dry-run` (estimate first), 
`--max-bytes-billed N` (hard cap), `--project ID` (exec project).
TIMESTAMP columns render in bq as `2026-10-13 12:00:00 UTC`, which the
bounds cannot parse — so ask for ISO instead:

```bash
cardio --bq 'SELECT text, FORMAT_TIMESTAMP("%Y-%m-%dT%H:%M:%SZ", ts) AS timestamp FROM prod.traces'
```

bq tips:

- **A default cost guard can live in the environment**:
  `CARDIO_MAX_BYTES=100GB` adds `--maximum_bytes_billed` to every bq run
  (binary units; `0` disables; the explicit `--max-bytes-billed` flag
  always wins) — a first "let me see what comes out" pass then fails
  fast instead of scanning 68GB uncontrolled.
- **The biggest lever is the SQL, not cardio.** SELECT exactly the
  fields you need (for example `jsonPayload.message`), and never pull
  whole payloads with `TO_JSON_STRING(payload)`. A wide SELECT costs
  more than any trim will save. cardio shines when the download is
  repetitive or broad; a narrow, trace-sized query is already optimal
  without it.
- `--max_rows=10000000` is passed to bq because bq silently caps
  selects at 1000 rows otherwise.
- Column names print once on stderr (`Note: bq columns: a | b | ...`).
  Rows join their fields with a tab, and empty or null columns keep
  their separator instead of vanishing.
- `--since`/`--until` are NOT pushed into bq, because the SQL is yours.
  Filter dates in the query with a `WHERE` clause over
  `FORMAT_TIMESTAMP(...)` text; the same ISO bounds still apply locally.

`--k8s`: a `kubectl logs` wrapper whose ONLY extra power is the pushdown
(`--since` becomes `--since-time` in UTC; kubectl runs with
`--timestamps` so `--until` can bind locally). The namespace and context
are kubectl's own (current ones). Pass `pod/name`, `deployment/name` or
a bare name; `-c` picks the container.

Any OTHER provider needs nothing from cardio — the trim engine reads
stdin:

```bash
aws logs tail /prod/eks --since 2h | cardio --stats
az webapp log tail | cardio --trace 'req_id=([a-z0-9-]+)'
journalctl -u my-service | cardio
```

Many questions over the same window? Download once, iterate free:

```bash
cardio --bq 'SQL' > cache.txt   # one download, trimmed
cardio cache.txt --query NTS    # local re-queries cost no fetch
```

## Flags

| Flag | Effect |
|---|---|
| `[file]` | Log file. If omitted, reads from stdin. |
| `--query TEXT` | Keep only lines containing TEXT (substring, applied first). |
| `--since TS` | Timestamp >= this. Accepts ISO (`2026-10-09T05:00:00`) or `YYYY-MM-DD` (day start). |
| `--until TS` | Timestamp < this. ISO, or `YYYY-MM-DD` (NEXT-day start, so the end day is kept). |
| `--tail N` | Keep only the LAST N kept lines: most recent wins, whole input reads. The lean default for files/stdin. |
| `--head N` | Keep only the FIRST N kept lines; the lean default for cloud fetchers. In `--trace`/`--summary` mode this is the event cap (earliest events win). |
| `--skip N` | Drop the first N kept lines before the window. bq pages as `--skip 100 --head 100` (page 2). |
| `--raw` | Skip the default pipeline: verbatim lines, no trim of timestamps or boilerplate, no dedup, no cap, no field cut. |
| `--dedup` | Force dedup in `--raw` mode (lean mode already has dedup on). |
| `--max-field-len N` | Cut any field (or whole line) longer than N chars, marked with "…". Lean default 1000; 0 = unlimited. |
| `--json` | NDJSON output: `{"index","content"}` per line; with `--trace`, `{"id","events"}`; with `--summary`, `{"id","count","first","last"}`. |
| `--trace REGEX` | Group lines by captured ID, for example `"request_id=([a-z0-9-]+)"`. Sessions print at the end. |
| `--summary` | With `--trace`: compact per-ID map with event count plus first and last events. IDs preserved. |
| `--top N` | In trace mode (full or summary): keep only the biggest N sessions; the rest drop with a note. |
| `--stats` | Survey mode: top 20 pattern frequencies on stderr, no stream output. Digit runs fold to `#` first, so `sid=123` and `sid=456` count as one pattern. `--stats --json` prints NDJSON `{"pattern","count"}` on stdout. |
| `--dedup-fold` | Third dedup level for censuses: lines differing only in digit runs collapse (digits become `#`, same fold as `--stats`); the run announces it on stderr. |
| `--group-col day,status` | bq census: count rows per column value — same output shape as `--stats` (NDJSON with `--json`). Rows are verbatim, so the column map matches the header. Only with `--bq`. |
| `--gcloud FILTER` | Stream Cloud Logging entries matching this filter. `--since`/`--until` pushed server-side. |
| `--bq SQL` | Run the bq CLI with this SQL and process its rows. Exactly one source at a time. |
| `--k8s SELECTOR` | kubectl logs target (`pod/x`, `deployment/x`, bare name); `--since` pushes to `--since-time`. |
| `-c, --container NAME` | With `--k8s`: the pod's container. |
| `--project ID` | bq only: the GCP project that runs the job (default = the CLI's configured project). |
| `--max-bytes-billed N` | bq only: hard cap on billed bytes (errors instead of scanning multi-TB tables). |
| `--dry-run` | bq only: cost estimate only, reads no rows. |
| `--bq-args "--flag=..."` | bq only: extra raw bq CLI flags (whitespace-split, no quotes inside). |
| `--timeout SECS` | Kill a hung fetcher after SECS (process-group kill; job id stays visible). Default 600 s on STREAMING fetchers (gcloud, k8s); bq waits whole jobs, so it needs an explicit `--timeout`. `0` disables. |
| `--config FILE` | TOML with `timestamp_regex`, `boilerplate_regex`, `default_trace`. |
| `--mcp` | Run as an MCP stdio server exposing these modes as tools (below). |
| `--agent-md` | Print the agent-facing markdown card (pipe it: `cardio --agent-md > AGENTS.md`). |

## MCP server and when to use it over the card

Two channels, one engine. The CARD (via `--agent-md` or the AGENTS.md you
dropped in your repo) teaches an agent that drives cardio through SHELL.
The MCP server lets an agent runtime that speaks MCP skip that reading
entirely: tools arrive through `tools/list` with the contract written
into each schema (what the lean pipeline drops, that the window follows
the source, that a wide bq scan should dry-run first). So:

- **Shell + card**: best when the agent composes freely (pipes, follow-up
  greps, `cache.txt` flows). One card read amortizes over a whole repo.
- **MCP**: best for runtimes where structured tools beat prose — schemas
  are always current because they ship with the binary itself (a shell
  can drift if you skip re-reading the card; the MCP tools cannot, since
  they are the same release).
- Both channels are honest the same way: what got dropped, field maps,
  job ids and caps arrive as explicit content, never silent. (WHY the
  server stays alive under load: the 1.x core ran ONE request at a
  time — a slow fetch froze the whole server, pings went unanswered and
  agents watched minutes turn to infinite. The 2.0 core never lets a
  tool call block the protocol.)

The thread model:

```
 stdin ──► READER thread ──► ping answered in µs, always
             │               (it never blocks behind a tool call)
             ├─ tools/call ─► WORKER thread (one per call)
             │                 ├─ both pipes drained concurrently
             │                 ├─ heartbeat every 10 s to the client
             │                 ├─ cancel ─► child's PROCESS GROUP killed
             │                 └─ max 2 heavy fetches; more QUEUE visibly
             └──► WRITER thread (owns stdout, ONE channel — no races)
                    └─ client's stdout dies ─► running fetches KILLED
```

The full reasoning (why std threads instead of an SDK, why process
groups, why keep-first caps) is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#5-mcp-core-reader--worker--writer).

### It is NOT Google-only

The headline tool is `fetch_command`: ANY log-producing CLI (Azure, AWS,
journalctl, docker logs — any provider, any agent runtime) is piped
straight through cardio's lean pipeline. The named fetchers (`fetch_bq`,
`fetch_gcloud`, `fetch_k8s`) exist ONLY to add provider-specific super
powers (cost guards, server-side pushdown); for everything else,
`fetch_command` is the tool.

`cardio --mcp` speaks MCP (Model Context Protocol) over stdio: line-based
JSON-RPC with tools/list and tools/call. Same binary, so the tools always
match the flags. Eight tools:

| tool | purpose |
|---|---|
| `read_log` | read a file through the lean pipeline (query/tail/head/skip/raw/json/field-cap) |
| `survey_log` | the `--stats --json` survey |
| `trace_log` | the grouping modes (trace/summary/top/head-cap) |
| `census_log` | bq census: rows-per-column-value counts (the `--group-col` semantics) |
| `fetch_command` | ANY provider CLI piped through cardio — the provider-neutral fetch |
| `fetch_bq` | bq SQL with the cost guards (dry-run, byte caps, project) |
| `fetch_gcloud` | Cloud Logging with pushdown bounds |
| `fetch_k8s` | kubectl logs with the since-time pushdown |

### The v2 server core (why it stays alive under load)

The 1.x server froze behind a slow tool call: one request at a time, the
read loop never returned, pings went unanswered, agents saw "minutes to
infinite". The v2 core is concurrent:

- one READ thread owns stdin — `ping` answers in microseconds even while
  a 20-minute bq runs
- one thread per `tools/call` — heavy fetches run in parallel (capped at
  2, the third QUEUES with visible progress instead of hanging mute)
- `notifications/cancelled` actually CANCELS: the fetch child (and its
  whole process group on unix, so grandchildren cannot hold the pipes)
  is killed on the spot and the run answers with an honest `isError`
  (on Windows only the direct child dies — a CLI that spawns children
  may keep them alive)
- progress notifications flow whenever the client passes a progress
  token (start + a heartbeat every 10 s) — "working" never looks like
  "hung" again
- both pipes are drained concurrently (a 1.0.0 run hung 30 minutes when
  they were read sequentially); oversized output is capped (8 MiB stdout
  / 64 KiB stderr) with an in-band truncation marker — the MCP layer is
  a context window, not a filesystem
- a dead client STOPS the money: if the client's stdout dies, the writer
  announces it once and every fetch in flight is KILLED (waiting for a
  20-minute bq to finish for nobody was the 1.x flaw)

Results carry the trimmed lines plus cardio's stderr notes (what was
dropped, bq column names, job ids) as separate text blocks; a nonzero
cardio exit comes back as an `isError` result, not a client crash.

For Claude Code, register it with:

```bash
claude mcp add cardio -- cardio --mcp
```

The agent then calls `read_log`/`survey_log`/`fetch_command`/... instead
of shelling out — same engine, structured tools.

## Performance

`./bench.sh` builds the release binary and times every mode on a
synthetic 1M-line log (50% duplicate lines, digit ids, huge XML
bodies). It uses hyperfine when installed and falls back to a plain
monotonic-clock loop. Local snapshot (M1, release build):

| mode | time per 1M lines |
|---|---|
| `cat` baseline | 0.01 s |
| `--query` | 0.11 s |
| `--raw` (no pipeline) | 0.21 s |
| `--trace` (raw, uncapped) | 0.39 s |
| `--stats` | 0.68 s |
| lean (trim + exact dedup + window + field cut) | 0.62 s |

The badge at the top shows the LIVE lean number, measured by CI on
github's ubuntu runners after every release (and on demand via
workflow_dispatch) — full per-mode tables at the badge's link. The
table above is one local M1 run for orientation; on other hardware,
run `./bench.sh` and read your own.

That is roughly 75 MB/s on the lean path end to end (the 1000-char
field cutoff included, byte-length fast path). One extra data point:
`--raw --dedup-fold` costs the same as raw (≈0.21 s) — the digit fold
is a regex over the line, not a scan. The full methodology and how the
machine works live in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). The trim runs as a
single combined regex pass. When the input comes from gcloud or bq, the
network dominates anyway. The bench's trace scenario runs with `--raw`
on purpose: in lean mode the session cap bounds the input early, so its
time would measure the cap rather than the grouper.

## Tests

```bash
cargo test    # unit + integration: the CLI contract (tests/cli.rs), a real MCP stdio drive (tests/mcp.rs), fake fetchers per OS (sh stubs on unix, .cmd stubs on Windows)
```

Philosophy: black-box, at the CLI level. Tests pin the promises the
agent card makes (trim behavior, dedup honesty, window semantics, exit
codes, stderr witnesses, JSON shapes). Because of that they survive
refactors, and any drift between docs and behavior fails loudly.
gcloud and bq are faked with PATH-stub shell scripts (marked
`#[cfg(unix)]`), so the whole suite runs offline on every OS. A
staleness test makes every clap flag appear in the agent card, so the
card can only go stale by leaving a flag out on purpose. Unit tests
exist only for pure math that a CLI test cannot probe sanely, such as
calendar rollovers and the 9999-12-31 clamp.

Coverage (cargo llvm-cov, the `coverage` job): ~89% and held ≥80% by the
gate of taste, not a hard rule. The MCP layer is measured separately:
its tests drive the spawned binary over stdio, which llvm tooling cannot
attribute to source — the layer is excluded from the ratio instead of
pretending it is untested.

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

# one pod's recent errors
cardio --k8s deployment/my-api --since 2026-10-09 --query error
```