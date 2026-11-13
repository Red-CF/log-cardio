## [2.0.0] - 2026-11-13

### 📚 Documentation

- ARCHITECTURE.md — sectioned how-it-works with diagrams and whys

### 💼 Other

- Concurrent MCP core — the server stays alive under load
- Collapse nested if (clippy -D warning on the CI's lint pass)
- Sectioned how-it-works — pipeline diagram + MCP thread model
- WHO is cardio for comes first
- Point the deep-dives at the source (docs/ARCHITECTURE.md stays personal)
- Keep the bench badge (the CI-live lean number) in the set
- One supported-platforms badge instead of four per-OS
- Single supported badge spells out both macOS archs
- Drop the before-and-after section (kept per user call)
- Reduced intro (fetch + card in two lines)
- Automatic (git-cliff), no hand-maintained file
- Stale CHANGELOG.md link -> the git-cliff file on gh-pages
## [1.1.0] - 2026-10-23

### 💼 Other

- MCP live fixes (deadlock, progress), fold dedup, env cost guard, census, coverage
## [1.0.0] - 2026-10-21

### ⚙️ Miscellaneous Tasks

- Version gate only for shippable code; release only for new versions
- Fail closed on PR-file listing and registry errors

### 💼 Other

- Live bench badge on gh-pages + job speedups
- Bench snapshot wording (local M1 table vs live CI badge)
- MCP when-and-why (card vs structured-tools trade-off)
- BENCH_JSON unbound under set -u (defaulted instead)
- Fetcher spawn retries .cmd/.bat shims on NotFound
## [0.2.0] - 2026-10-15

### 🧪 Testing

- Restore cfg(unix) gates my new helpers displaced (windows CI)

### ⚙️ Miscellaneous Tasks

- Rename clippy job to lint
- Cargo lock refresh
- Prebuilt macOS Intel binary via cross-target build

### 💼 Other

- 0.2.0 — module restructure + verbatim --raw, bq cost kit, tab CSV transport
- OS badges, first-run guidance, test philosophy, first bench results
- Restore badge block (lost in the full rewrite); per-OS release badges with real arch
- Dependabot (cargo + actions) and RustSec audit check in CI
## [0.1.4] - 2026-10-13

### 🚀 Features

- Add some readme specifications.
## [0.1.2] - 2026-10-13

### 🚀 Features

- Initial steps of the project first functonalities to work with massive logs and generate an output
- Added Gcloud integration and made some code review improvements
- --summary per-ID map (--trace), fix review findings, full README
- --bq SQL — BigQuery as a fetcher into the same pipeline

### 🐛 Bug Fixes

- Review round — real ISO validation, stats/summary note, test hardening

### 🚜 Refactor

- Pay structure debt — per-mode runners + shared line pipeline

### 💼 Other

- Split tests and release builds into parallel jobs
- Auto-bump patch when the merged version is already on crates.io
