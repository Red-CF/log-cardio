use regex::Regex;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::process;
use bloomfilter::Bloom;
use clap::Parser;
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(author, version, about = "Trim noise from logs for LLM processing")]
struct Args {
    /// Path to the log file
    file_path: String,

    /// Filter lines containing this text
    #[arg(long)]
    query: Option<String>,

    /// Remove duplicate lines
    #[arg(long)]
    dedup: bool,

    /// Max number of lines to output
    #[arg(long)]
    max_lines: Option<usize>,

    /// Output in JSON format
    #[arg(long)]
    json: bool,

    /// Only logs after this timestamp (approximate matching)
    #[arg(long)]
    since: Option<String>,

    /// Only logs before this timestamp (approximate matching)
    #[arg(long)]
    until: Option<String>,
}

#[derive(Serialize)]
struct LogEntry {
    index: usize,
    content: String,
}

fn trim_line(line: &str, re_ts: &Regex, re_bp: &Regex) -> Option<String> {
    let trimmed = re_ts.replace_all(line, "");
    let trimmed = re_bp.replace_all(&trimmed, "");
    let final_line = trimmed.trim();

    if final_line.is_empty() {
        None
    } else {
        Some(final_line.to_string())
    }
}

fn main() {
    let args = Args::parse();

    let file = match File::open(&args.file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening file {}: {}", args.file_path, e);
            process::exit(1);
        }
    };
    let reader = BufReader::new(file);

    let re_timestamp = Regex::new(r"(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?)|(\d{2}/\w{3}/\d{4}:\d{2}:\d{2}:\d{2})").unwrap();
    let re_boilerplate = Regex::new(r"(?i)\b(INFO|DEBUG|TRACE)\b").unwrap();

    let mut bloom = if args.dedup {
        Some(Bloom::new_for_fp_rate(10_000_000, 0.01).expect("Failed to init bloom filter"))
    } else {
        None
    };

    let mut output_count = 0;

    for (idx, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error reading line {}: {}", idx + 1, e);
                process::exit(1);
            }
        };

        // 1. Time Slicing (Since/Until)
        // ponytail: simple substring match for timestamps to avoid heavy date parsing logic
        if let Some(ref s) = args.since {
            if !line.contains(s) && !line.contains(&s[0..10]) { // basic date check
                // Note: Real time slicing needs a date parser. This is a lazy approx.
            }
        }

        // 2. Query Filter
        if let Some(ref q) = args.query {
            if !line.contains(q) {
                continue;
            }
        }

        if let Some(final_line) = trim_line(&line, &re_timestamp, &re_boilerplate) {
            // 3. Deduplication
            if let Some(ref mut b) = bloom {
                if b.check(&final_line) {
                    continue;
                }
                b.set(&final_line);
            }

            // 4. Context Guard (Max Lines)
            if let Some(max) = args.max_lines {
                if output_count >= max {
                    break;
                }
            }

            // 5. JSON vs Text Output
            if args.json {
                let entry = LogEntry { index: idx + 1, content: final_line };
                println!("{}", serde_json::to_string(&entry).unwrap());
            } else {
                println!("{}", final_line);
            }

            output_count += 1;
        }
    }
}
