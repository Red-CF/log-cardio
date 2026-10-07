# Log Trimmer CLI

Fast Rust tool to strip noise from logs before LLM processing.

## Install
```bash
cargo install --path .
```

## Usage
```bash
log-trimmer path/to/google.log > trimmed.txt
# or pipe to an agent
log-trimmer path/to/google.log | claude-cli "Analyze these errors"
```
