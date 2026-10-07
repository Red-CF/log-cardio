use regex::Regex;
use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::process;

struct Config {
    file_path: String,
    query: Option<String>,
    dedup: bool,
}

/// Trims known noise from a log line.
/// ponytail: Returns Option to avoid allocating empty strings.
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
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <log_file> [--query <text>] [--dedup]", args[0]);
        process::exit(1);
    }

    let mut config = Config {
        file_path: args[1].clone(),
        query: None,
        dedup: false,
    };

    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--query" => {
                if i + 1 < args.len() {
                    config.query = Some(args[i + 1].clone());
                    i += 1;
                }
            }
            "--dedup" => config.dedup = true,
            _ => {}
        }
        i += 1;
    }

    let file = match File::open(&config.file_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Error opening file {}: {}", config.file_path, e);
            process::exit(1);
        }
    };
    let reader = BufReader::new(file);

    let re_timestamp = Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}.*?Z").unwrap();
    let re_boilerplate = Regex::new(r"(?i)(INFO|DEBUG|TRACE)").unwrap();

    let mut seen = HashSet::new();

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Error reading line: {}", e);
                process::exit(1);
            }
        };

        if let Some(ref q) = config.query {
            if !line.contains(q) {
                continue;
            }
        }

        if let Some(final_line) = trim_line(&line, &re_timestamp, &re_boilerplate) {
            if config.dedup {
                // Fixed: check existence before cloning for HashSet insertion
                if seen.contains(&final_line) {
                    continue;
                }
                seen.insert(final_line.clone());
            }
            println!("{}", final_line);
        }
    }
}
