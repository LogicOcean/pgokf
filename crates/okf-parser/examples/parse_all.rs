// SPDX-License-Identifier: AGPL-3.0-only
//! Parse-all latency harness for the OKF format benchmark (YAML leg).
//!
//! Recursively walks a corpus directory and times parsing every `.md` file
//! through [`okf_parser::parse_concept`] — the exact entry point the
//! `PostgreSQL` sync engine drives per file. It reports the file count, the
//! wall-clock elapsed for the read-plus-parse loop, derived throughput, and a
//! peak resident-set figure read from `/proc/self/status` (`VmHWM`).
//!
//! The directory walk uses only [`std::fs`] recursion (no `walkdir`), and the
//! bundle-relative path handed to the parser is the file's path with the
//! corpus root stripped — matching how the sync layer keys concepts.
//!
//! Usage:
//!
//! ```text
//! cargo run --release --example parse_all -- <corpus-dir>
//! ```

// The throughput/RSS figures are display-only: casting the byte/file counts to
// f64 for MiB and files-per-second is intentional and its precision loss is
// irrelevant to a human-readable benchmark line.
#![allow(clippy::cast_precision_loss)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use okf_parser::{ParserLimits, is_reserved_path, parse_concept};

/// Collected outcome of parsing the whole corpus.
#[derive(Debug, Default)]
struct Totals {
    files: usize,
    parsed_ok: usize,
    parse_errors: usize,
    skipped_reserved: usize,
    source_bytes: u64,
    // Accumulated so the optimizer cannot elide the parse work.
    link_count: u64,
    body_bytes: u64,
}

/// Recursively collect every `.md` file under `root`, in sorted order for
/// deterministic iteration.
fn collect_markdown(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)?
            .map(|entry| entry.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        for path in entries {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Read the peak resident set size (`VmHWM`) from `/proc/self/status`, in
/// kilobytes, when the platform exposes it.
fn peak_rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse::<u64>()
                .ok();
        }
    }
    None
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(corpus) = args.next() else {
        eprintln!("usage: parse_all <corpus-dir>");
        std::process::exit(2);
    };
    let root = PathBuf::from(&corpus);
    if !root.is_dir() {
        eprintln!("corpus path is not a directory: {corpus}");
        std::process::exit(2);
    }

    let files = collect_markdown(&root).unwrap_or_else(|error| {
        eprintln!("failed to walk corpus {corpus}: {error}");
        std::process::exit(1);
    });

    let limits = ParserLimits::default();
    let mut totals = Totals::default();
    let mut first_errors: Vec<String> = Vec::new();

    let start = Instant::now();
    for path in &files {
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let relative_text = relative.to_string_lossy().into_owned();
        if is_reserved_path(&relative_text) {
            totals.skipped_reserved += 1;
            continue;
        }
        totals.files += 1;

        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                totals.parse_errors += 1;
                if first_errors.len() < 5 {
                    first_errors.push(format!("{relative_text}: read failed: {error}"));
                }
                continue;
            }
        };
        totals.source_bytes += bytes.len() as u64;

        match parse_concept(&bytes, relative, limits) {
            Ok(concept) => {
                totals.parsed_ok += 1;
                totals.link_count += concept.links.len() as u64;
                totals.body_bytes += concept.body_text.len() as u64;
            }
            Err(error) => {
                totals.parse_errors += 1;
                if first_errors.len() < 5 {
                    first_errors.push(format!("{relative_text}: {error}"));
                }
            }
        }
    }
    let elapsed = start.elapsed();

    let seconds = elapsed.as_secs_f64();
    let mib = totals.source_bytes as f64 / (1024.0 * 1024.0);
    let files_per_sec = if seconds > 0.0 {
        totals.files as f64 / seconds
    } else {
        f64::INFINITY
    };
    let mib_per_sec = if seconds > 0.0 {
        mib / seconds
    } else {
        f64::INFINITY
    };

    println!("okf-parser parse_all");
    println!("corpus_dir      {corpus}");
    println!("files_total     {}", totals.files);
    println!("parsed_ok       {}", totals.parsed_ok);
    println!("parse_errors    {}", totals.parse_errors);
    println!("skipped_reserved {}", totals.skipped_reserved);
    println!("source_bytes    {}", totals.source_bytes);
    println!("source_mib      {mib:.2}");
    println!("elapsed_seconds {seconds:.4}");
    println!("files_per_sec   {files_per_sec:.1}");
    println!("mib_per_sec     {mib_per_sec:.1}");
    match peak_rss_kib() {
        Some(kib) => println!("peak_rss_mib    {:.1}", kib as f64 / 1024.0),
        None => println!("peak_rss_mib    unavailable"),
    }
    // Emit the accumulators so the read/parse work is observable and not
    // optimized away.
    println!("total_links     {}", totals.link_count);
    println!("total_body_bytes {}", totals.body_bytes);

    if !first_errors.is_empty() {
        eprintln!("first parse errors:");
        for error in &first_errors {
            eprintln!("  {error}");
        }
    }

    if totals.parse_errors > 0 {
        std::process::exit(1);
    }
}
