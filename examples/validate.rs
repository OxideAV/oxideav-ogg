//! Validate an Ogg file against the RFC 3533 page-structure rules:
//!
//! ```sh
//! cargo run --example validate -- file.ogg [more.ogg ...]
//! ```
//!
//! Runs [`oxideav_ogg::validate::validate`] over each file and prints
//! the typed conformance report — whole-file tallies (pages, logical
//! bitstreams, chain links, junk bytes) followed by one line per rule
//! violation with its severity, byte offset, page ordinal, serial,
//! and detail. The process exits non-zero when any file has at least
//! one `Severity::Error` issue (warnings alone keep the exit clean),
//! so the example doubles as a shell-scriptable conformance checker.

use oxideav_ogg::validate::{validate, Severity};

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        eprintln!("usage: validate <file.ogg> [more.ogg ...]");
        std::process::exit(2);
    }

    let mut any_error = false;
    for file in &files {
        let bytes = match std::fs::read(file) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{file}: cannot read: {e}");
                any_error = true;
                continue;
            }
        };
        let report = validate(&bytes);
        if report.is_clean() {
            println!(
                "{file}: OK — {} pages, {} streams, {} links",
                report.pages, report.streams, report.links
            );
        } else {
            println!("{file}: {report}");
            if report.issues.iter().any(|i| i.severity == Severity::Error)
                || report.suppressed_issues > 0
            {
                any_error = true;
            }
        }
    }
    if any_error {
        std::process::exit(1);
    }
}
