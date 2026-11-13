# Architecture

How cardio works, layer by layer — with the diagrams and the WHYS. The
README sells the tool; this file explains the machine. Meant to be read
once with the source open.

## 1. The big picture

Two programs share a binary: the CLI (any shell invocation) and the MCP
server (`--mcp`). Same engine, two front doors.

```
                    ┌────────────────────────────────────────┐
   shell / MCP ───► │  Args → validate → ONE source choice   │
                    └───────────────┬────────────────────────┘
                                    │
          ┌─────────────────────────┼──────────────────────┐
          ▼                         ▼                      ▼
   source.rs (fetchers)      file / stdin            kubectl | bq | gcloud
   cost guards, pushdown,    BufRead / CSV rows      child with
   process groups            (csv crate)             its own process group
          └─────────────────────────┼──────────────────────┘
                                    ▼
                            pipeline.rs (per line):
                            query → range filter → trim → dedup → clamp
                                    │
              ┌─────────────────────┼───────────────────┐
              ▼                     ▼                   ▼
         stream (run_stream)   sessions.rs         stats.rs
         window by source      trace grouping      census (--group-col)
```

The rule that keeps the layers honest: **data flows one way**. A layer
never reaches up; the orchestration (main.rs) wires and the notes layer
(notes.rs) reports afterwards what was given up.

## 2. The per-line pipeline

```
line ─┬─ query reject (substring, cheapest first)
      ├─ range filter (first FULL-ISO ts in the line →
      │   canonical compare; ts-less lines bypass with a one-time warn)
      ├─ trim (ONE combined regex: ts | boilerplate → "")
      ├─ dedup (exact HashSet ≤5000 unique → bloom after, switch announced)
      ├─ clamp (--max-field-len, byte-length fast path)
      ▼
   window (HEAD for fetchers / TAIL for streams) → emit
```

WHYs that came from real failures, not design taste:

- **One combined trim regex** instead of two passes: the two-pass trim
  was ~30% of total runtime per million lines (measured with bench.sh —
  the lean/raw gap showed it).
- **Dedup: exact-then-bloom.** Two agent runs reported distrust in the
  ~1% bloom false positives. The hybrid keeps TRUE exactness for the
  first 5000 unique lines (where the trust lives) and pays probabilistic
  cost only on huge noisy inputs. The exact set survives the switch, so
  a line seen early still collides later — no re-emissions, no drift.
- **Window side follows the source.** `--bq 'SELECT ... ORDER BY ASC'`
  + a tail cap silently discarded the OLDEST days (a real agent lost
  two days of its own query). For fetchers, the head IS the caller's
  intent; for live streams, the tail IS the news.
- **--max-field-len default in lean.** JWTs and request bodies passed
  whole on every line; 1000 chars + a "…" marker costs nothing on the
  byte fast path and pays for itself in one trace.

## 3. Timestamps: the single-invariant zone

`timestamps.rs` owns everything time-shaped:

```
line/bound ──► is_full_iso (shape + calendar sanity)
           ──► to_canonical (space→T, tz strip, pad to MAX_FRAC digits)
           ──► assert_canonical_shape (ONE gate)
           ──► lexicographic compare (string order == time order)
```

Two properties make this safe to touch:

1. **One writer**: every compareable instant comes from `to_canonical`,
   and every exit asserts the canonical shape. A bug cannot mis-order
   silently — it panics loud, naming the string.
2. **[0-9] over \d**: regex `\d` is Unicode-aware; every byte-level
   arithmetic assumes ASCII digits. The grammar family (FRAC, ISO_BODY,
   TRIM_BODY) is one static tree, so widening it widens both the matcher
   and the trim consistently.

Pushdown literals for gcloud/kubectl are derived through the same
canonical path (`server_ts`, `until_server_bound`) — the server side and
the local filter cannot disagree about what a bound means.

## 4. Fetchers: cost and safety are the product

`source.rs` owns every external spawn. The pattern per fetcher:

```
spawn (own process group, unix) ─► pipes ─► cost guards (bq only:
--dry-run, --maximum_bytes_billed, CARDIO_MAX_BYTES env, --bq-args)
```

Whys:

- **Process groups on unix.** A fetcher that spawns children (a `sleep`
  inside a wrapper, gcloud's own subprocesses) — a bare kill leaves the
  grandchildren holding the output pipes, and cancel/cleanup block until
  they die of old age. A real 25-second cancellation caught this.
- **Streaming timeout ≠ jq timeout.** gcloud/kubectl streams die on a
  flow-stop (default 600 s); bq JOBS sit silent while working, so bq
  requires an explicit timeout — the same wall there kills legitimate
  scans.
- **CARDIO_MAX_BYTES env.** First-pass exploratory scans failed fast
  with money guards nobody had to remember; `0` disables, and the
  explicit flag always wins.

## 5. MCP core: reader / worker / writer

This is where 1.x died and 2.0 exists: the old core ran ONE request at a
time, so a slow fetch froze the whole server — pings unanswered,
cancellation unread, agents watching minutes turn to infinite.

```
 stdin ──► READER thread (owns stdin; NEVER blocks on a tool)
             │  ping ──────────► answered in microseconds, always
             │  tools/list ────► answered
             │  tools/call ────► registry + spawn WORKER thread, loop on
             │  cancelled ─────► flip the call's flag (KILLS its child)
             ▼
        WORKER thread (one per call)
             │  both pipes drained concurrently on capture threads
             │  heartbeat every 10 s (progress when the client asks)
             │  hard ceiling 3 × 600 s (nothing runs to infinity)
             │  cancel → kill PROCESS GROUP → honest isError
             ▼
        WRITER thread (owns stdout; ONE channel in)
             │  responses + notifications serialized, no races
             │  dead pipe → CLIENT_GONE set once → running fetches KILLED
             └─ fetch budget: max 2 concurrent heavy fetches;
                the third QUEUES with visible progress
```

Whys:

- **std threads, not tokio / rmcp.** The needed protocol surface is 3
  methods + notifications over TWO file descriptors. The SDK buys
  multi-transport (SSE/HTTP), typed schemas and an async runtime —
  none of that is ours. The migration ceiling is written in the code:
  rmcp earns its keep when HTTP transport or dynamic resources land.
- **ONE writer.** JSON-RPC responses interleaving from N threads is the
  classic corruption: every byte of stdout flows through a single
  channel receiver.
- **Cancellation is real, not advisory.** The registry maps request id →
  per-call flag (a Vec per id: a REUSED id must not lose call 1's flag
  when call 2 cleans up — caught by review). The worker kills the whole
  process group on cancel, and `client_gone` (writer saw a dead pipe)
  kills running fetches too — no cloud scans for nobody.
- **Heavy-fetch budget = 2.** More than that means unbounded parallel
  cloud spend; the third call queues with visible progress ("queued
  behind 2") instead of hanging mute. Local FILE tools are exempt: a
  10-ms read never queues behind a 20-minute scan.
- **Keep-first capture caps** (8 MiB stdout / 64 KiB stderr). Both pipes
  drain to EOF ALWAYS — stopping at the cap blocks the child on a full
  pipe, which was the 30-minute hang of 1.0. The tail is discarded, not
  held: the MCP layer is a context window, not a filesystem.
- **fetch_command uses the GENUINE sibling cardio**, never CARDIO_BIN
  (that would run the provider script as "cardio" itself, twice, with
  no trimming).

## 6. Test philosophy (why black-box)

`tests/cli.rs` + `tests/mcp.rs` pin the PROMISES the agent card makes —
trim, dedup honesty, window semantics, exit codes, stderr witnesses,
JSON shapes, protocol behavior (ping under load, cancellation, deadlock
regressions). They survive refactors because they touch only the
binary's surface. Fetchers are faked per OS (sh stubs on unix, .cmd on
Windows) so the suite runs offline everywhere. The staleness test makes
every clap flag appear in the agent card. Unit tests exist only where a
CLI test cannot probe (calendar rollovers, the 9999 clamp).

Coverage (cargo llvm-cov): ~89% with the gate of taste. The MCP layer is
excluded from the ratio because its tests drive the spawned binary —
llvm tooling cannot attribute subprocess coverage, and counting it as
0% would lie.

## 7. Bench methodology

`bench.sh` generates a synthetic 1M-line log (≈50% duplicates for the
dedup class, digit ids for the stats fold, 200-char XML bodies for the
field cap) and times every mode with hyperfine (monotonic fallback).
The lean number INCLUDES the full default pipeline — trim + exact dedup
+ window + field cut. Numbers below are one local M1 run; the CI badge
(number in the README) re-measures on ubuntu runners after every
release so the runs stay comparable to each other.

## 8. Time handling

ISO timestamps compare lexicographically, so string order equals time
order. Timezone offsets are matched and trimmed for comparison, but the
comparison is tz-naive: mixing `+02:00`-suffixed logs with UTC bounds
can misjudge hours around day boundaries.

## 9. A note on the 1.x server

The 1.x MCP core ran one request at a time: a slow fetch froze the whole
read loop, pings went unanswered, cancellation could not arrive, and
both pipes were captured sequentially (a dual-stream bq run deadlocked
for 30 real minutes). The whole v2 core in section 5 exists to fix
exactly that class of defect; the release history is generated by git-cliff at release time (gh-pages CHANGELOG.md).
