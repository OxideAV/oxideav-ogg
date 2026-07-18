#![no_main]

//! Whole-file conformance validator hardening harness.
//!
//! `oxideav_ogg::validate::validate` is documented to never panic on
//! arbitrary bytes and to keep its memory bounded. This target holds
//! it to that on attacker input:
//!
//! * the walk must return for any byte sequence — junk, truncated
//!   pages, CRC-damaged pages, fake `OggS` captures inside payloads,
//!   hostile segment tables;
//! * the issue list must respect the `MAX_ISSUES` retention cap and
//!   the junk tally can never exceed the input length;
//! * the report must be deterministic — a second walk over the same
//!   bytes produces identical counts;
//! * the report's `Display` rendering (used verbatim in test-failure
//!   messages) must itself not panic.

use libfuzzer_sys::fuzz_target;
use oxideav_ogg::validate::{validate, MAX_ISSUES};

fuzz_target!(|data: &[u8]| {
    let report = validate(data);
    assert!(
        report.issues.len() <= MAX_ISSUES,
        "issue list exceeded its retention cap"
    );
    assert!(
        report.junk_bytes <= data.len() as u64,
        "junk tally exceeds the input length"
    );

    // Determinism: same bytes, same findings.
    let again = validate(data);
    assert_eq!(report.pages, again.pages);
    assert_eq!(report.streams, again.streams);
    assert_eq!(report.links, again.links);
    assert_eq!(report.junk_bytes, again.junk_bytes);
    assert_eq!(report.issues.len(), again.issues.len());
    assert_eq!(report.suppressed_issues, again.suppressed_issues);

    // The Display path allocates per issue; it must hold up too.
    let _ = report.to_string();
});
