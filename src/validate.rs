//! Whole-file RFC 3533 conformance validation.
//!
//! [`validate`] walks a complete physical Ogg bitstream and checks every
//! normative page-structure rule of RFC 3533 (`docs/container/ogg/
//! rfc3533-ogg.txt`), producing a typed [`ConformanceReport`] instead of
//! failing on the first problem:
//!
//! * **§6 field 1** — every page begins with the `OggS` capture pattern;
//!   bytes between pages are junk ([`Rule::CapturePattern`]).
//! * **§6 field 2** — `stream_structure_version` is 0 ([`Rule::Version`]).
//! * **§6 fields 8–9** — the segment table and body must fit inside the
//!   buffer; a file ending mid-page is truncated ([`Rule::Truncated`]).
//! * **§6 field 7** — the 32-bit CRC (polynomial 0x04c11db7, CRC field
//!   zeroed) must match ([`Rule::CrcMismatch`]).
//! * **§4** — each logical bitstream starts with exactly one BOS page and
//!   ends with an EOS page; grouped BOS pages appear together before any
//!   data page; serial numbers are unique within the whole physical
//!   bitstream, across grouping *and* chaining ([`Rule::MissingBos`],
//!   [`Rule::DuplicateBos`], [`Rule::SerialReuse`],
//!   [`Rule::BosNotContiguous`], [`Rule::MissingEos`],
//!   [`Rule::PageAfterEos`]).
//! * **§6 field 6** — `page_sequence_number` increases by one per page on
//!   each logical bitstream separately ([`Rule::SequenceGap`],
//!   [`Rule::SequenceRegression`], [`Rule::SequenceStart`]).
//! * **§6 field 4** — the granule position of pages that complete packets
//!   never decreases within a logical bitstream, `-1` is reserved for
//!   pages on which no packet finishes, and a page that finishes no
//!   packet must carry `-1` ([`Rule::GranuleRegression`],
//!   [`Rule::SpuriousGranule`], [`Rule::MissingGranule`]).
//! * **§5 / §6 field 3** — the `continued` flag agrees with the previous
//!   page's lacing (a page after a completed packet is fresh; a page
//!   after a 255-lacing tail is continued), a BOS page never continues a
//!   packet, and an EOS page never ends mid-packet
//!   ([`Rule::ContinuedWithoutPartial`], [`Rule::PartialNotContinued`],
//!   [`Rule::ContinuedBos`], [`Rule::EosMidPacket`]).
//!
//! The validator is damage-tolerant in the same spirit as
//! [`crate::demux`]: after junk or a CRC failure it rescans for the next
//! checksum-valid page and keeps going, so a single flipped bit yields
//! one precise issue instead of a wall of cascading noise. It never
//! panics on arbitrary bytes and its memory use is bounded — the issue
//! list is capped ([`MAX_ISSUES`]) with an overflow tally, and no page
//! body is ever copied.
//!
//! This is the muxer's CI gate: every integration test that produces a
//! physical bitstream asserts `validate(&bytes).is_clean()`.

use crate::crc;
use crate::page::flags;

/// Maximum size of one page in bytes: a 27-byte header, a full 255-entry
/// segment table, and 255 × 255 body bytes (RFC 3533 §6: "Pages are of
/// variable size, usually 4-8 kB, maximum 65307 bytes").
pub const MAX_PAGE_SIZE: usize = 27 + 255 + 255 * 255;

/// Cap on the number of [`Issue`]s a report retains. Hostile inputs can
/// manufacture one issue per page indefinitely; past the cap the report
/// keeps counting ([`ConformanceReport::suppressed_issues`]) without
/// allocating.
pub const MAX_ISSUES: usize = 1024;

/// How serious a rule violation is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// The file deviates from RFC 3533 in a way a strict reader may
    /// reject (violates a MUST-level structure rule).
    Error,
    /// The file is questionable but interoperable readers generally
    /// accept it (conventions the RFC states descriptively).
    Warning,
}

/// The RFC 3533 page-structure rule an [`Issue`] violates.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// Bytes that are not part of any page: no `OggS` capture pattern
    /// where a page must begin (§6 field 1).
    CapturePattern,
    /// `stream_structure_version` is not 0 (§6 field 2).
    Version,
    /// The buffer ends inside a page header, segment table, or body
    /// (§6 fields 8–9: the header declares the full page extent).
    Truncated,
    /// The page checksum does not match the page bytes (§6 field 7).
    CrcMismatch,
    /// A data page arrived for a serial that never had a BOS page — every
    /// logical bitstream "starts with a special start page" (§4).
    MissingBos,
    /// A second BOS page arrived for a serial that is still live in the
    /// current link (§4: one BOS per logical bitstream).
    DuplicateBos,
    /// A BOS page reuses the serial of a logical bitstream that already
    /// ended — "Each chained logical bitstream MUST have a unique serial
    /// number within the scope of the physical bitstream" (§4, stated for
    /// grouped bitstreams too).
    SerialReuse,
    /// A new logical bitstream's BOS page arrived after the current
    /// link's data pages began — "all bos pages of all logical bitstreams
    /// MUST appear together at the beginning of the Ogg bitstream" (§4).
    BosNotContiguous,
    /// A logical bitstream was never closed: no page with the EOS flag
    /// before the file (or its link) ended — every logical bitstream
    /// "ends with a special page (eos)" (§4).
    MissingEos,
    /// A page arrived for a logical bitstream after its EOS page (§4:
    /// the EOS page is "the final page of a logical bitstream").
    PageAfterEos,
    /// A BOS page carries the `continued` flag — the first page of a
    /// logical bitstream cannot continue a packet (§6 field 3).
    ContinuedBos,
    /// A logical bitstream's first page has a nonzero
    /// `page_sequence_number`. §6 field 6 requires the sequence to
    /// increase per page; starting at 0 is the universal convention.
    SequenceStart,
    /// `page_sequence_number` jumped forward — one or more pages of this
    /// logical bitstream were lost (§6 field 6: the sequence number lets
    /// "the decoder identify page loss").
    SequenceGap,
    /// `page_sequence_number` repeated or went backwards (§6 field 6:
    /// "this sequence number is increasing on each logical bitstream
    /// separately").
    SequenceRegression,
    /// The granule position of a packet-completing page is lower than an
    /// earlier packet-completing page of the same logical bitstream
    /// (§6 field 4 / Appendix A: "an increasing position number for a
    /// specific logical bitstream").
    GranuleRegression,
    /// A page on which no packet finishes carries a granule position
    /// other than -1 (§6 field 4: "A special value of -1 ... indicates
    /// that no packets finish on this page").
    SpuriousGranule,
    /// A page on which packets DO finish carries granule position -1,
    /// contradicting the §6 field 4 semantics of the reserved value.
    MissingGranule,
    /// The page's `continued` flag is set but the previous page of this
    /// logical bitstream ended with a completed packet (§6 field 3:
    /// "set: page contains data of a packet continued from the previous
    /// page").
    ContinuedWithoutPartial,
    /// The previous page of this logical bitstream ended mid-packet (a
    /// 255 lacing value) but this page's `continued` flag is unset
    /// (§6 field 3: "unset: page contains a fresh packet").
    PartialNotContinued,
    /// An EOS page ends on a 255 lacing value — the packet it opens can
    /// never complete (§5: a lacing value below 255 "marks the end of
    /// the packet").
    EosMidPacket,
}

impl Rule {
    /// The severity class of a violation of this rule.
    #[must_use]
    pub fn severity(self) -> Severity {
        match self {
            // Conventions the RFC states descriptively rather than as
            // MUST-level structure: interoperable readers accept these.
            Rule::SequenceStart | Rule::MissingGranule => Severity::Warning,
            _ => Severity::Error,
        }
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Rule::CapturePattern => "capture-pattern",
            Rule::Version => "version",
            Rule::Truncated => "truncated",
            Rule::CrcMismatch => "crc-mismatch",
            Rule::MissingBos => "missing-bos",
            Rule::DuplicateBos => "duplicate-bos",
            Rule::SerialReuse => "serial-reuse",
            Rule::BosNotContiguous => "bos-not-contiguous",
            Rule::MissingEos => "missing-eos",
            Rule::PageAfterEos => "page-after-eos",
            Rule::ContinuedBos => "continued-bos",
            Rule::SequenceStart => "sequence-start",
            Rule::SequenceGap => "sequence-gap",
            Rule::SequenceRegression => "sequence-regression",
            Rule::GranuleRegression => "granule-regression",
            Rule::SpuriousGranule => "spurious-granule",
            Rule::MissingGranule => "missing-granule",
            Rule::ContinuedWithoutPartial => "continued-without-partial",
            Rule::PartialNotContinued => "partial-not-continued",
            Rule::EosMidPacket => "eos-mid-packet",
        };
        f.write_str(s)
    }
}

/// One rule violation found by [`validate`].
#[derive(Clone, Debug)]
pub struct Issue {
    /// The rule violated.
    pub rule: Rule,
    /// [`Rule::severity`] of `rule`, denormalised for convenience.
    pub severity: Severity,
    /// Byte offset in the input where the violation was observed (the
    /// start of the offending page, the first junk byte, or the input
    /// length for end-of-file issues such as [`Rule::MissingEos`]).
    pub byte_offset: u64,
    /// Ordinal of the offending page among the structurally accepted
    /// pages (0-based, physical order), when the issue is tied to one.
    pub page_index: Option<u64>,
    /// `bitstream_serial_number` the issue concerns, when known. For a
    /// CRC-mismatched page this is the *claimed* serial of the
    /// untrusted header.
    pub serial: Option<u32>,
    /// Human-readable specifics (expected/actual values).
    pub detail: String,
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?}] {} @ byte {}",
            self.severity, self.rule, self.byte_offset
        )?;
        if let Some(p) = self.page_index {
            write!(f, " (page {p})")?;
        }
        if let Some(s) = self.serial {
            write!(f, " (serial {s:#010x})")?;
        }
        write!(f, ": {}", self.detail)
    }
}

/// The typed result of [`validate`]: whole-file statistics plus every
/// rule violation found (capped at [`MAX_ISSUES`]).
#[derive(Clone, Debug, Default)]
pub struct ConformanceReport {
    /// Structurally accepted pages (capture pattern + intact extent +
    /// matching CRC; version-0 layout).
    pub pages: u64,
    /// Logical bitstreams observed (BOS pages plus bitstreams implied by
    /// orphan data pages), totalled across all chain links.
    pub streams: u32,
    /// Chain links observed (1 for an unchained file, 0 for a file with
    /// no pages).
    pub links: u32,
    /// Bytes that belong to no page: leading/trailing/inter-page junk
    /// spans reported under [`Rule::CapturePattern`].
    pub junk_bytes: u64,
    /// Every violation found, in file order, capped at [`MAX_ISSUES`].
    pub issues: Vec<Issue>,
    /// Violations found beyond the [`MAX_ISSUES`] cap (counted, not
    /// retained).
    pub suppressed_issues: u64,
}

impl ConformanceReport {
    /// True when the file violated no rule at all (no errors, no
    /// warnings, nothing suppressed).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty() && self.suppressed_issues == 0
    }

    /// Number of [`Severity::Error`] issues retained.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .count()
    }

    /// Number of [`Severity::Warning`] issues retained.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
            .count()
    }

    /// True when at least one retained issue violates `rule`.
    #[must_use]
    pub fn has(&self, rule: Rule) -> bool {
        self.issues.iter().any(|i| i.rule == rule)
    }

    /// The retained issues violating `rule`, in file order.
    pub fn of_rule(&self, rule: Rule) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(move |i| i.rule == rule)
    }

    fn push(&mut self, issue: Issue) {
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(issue);
        } else {
            self.suppressed_issues += 1;
        }
    }
}

impl std::fmt::Display for ConformanceReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{} pages, {} streams, {} links, {} junk bytes: {} error(s), {} warning(s){}",
            self.pages,
            self.streams,
            self.links,
            self.junk_bytes,
            self.error_count(),
            self.warning_count(),
            if self.suppressed_issues > 0 {
                format!(" (+{} suppressed)", self.suppressed_issues)
            } else {
                String::new()
            }
        )?;
        for issue in &self.issues {
            writeln!(f, "  {issue}")?;
        }
        Ok(())
    }
}

/// Per-logical-bitstream model while walking the file.
struct StreamModel {
    /// `page_sequence_number` of the last accepted page.
    last_seq: u32,
    /// Granule position of the last packet-completing page (`None`
    /// until one is seen).
    last_granule: Option<i64>,
    /// The last accepted page ended on a 255 lacing value: a packet is
    /// open across the page boundary.
    open_packet: bool,
    /// An EOS page was accepted for this bitstream.
    eos: bool,
    /// A [`Rule::PageAfterEos`] issue was already reported; suppress
    /// repeats for the same bitstream.
    after_eos_reported: bool,
    /// The previous page claiming this serial failed its CRC and was
    /// skipped, so sequence/continuity/granule state is unknowable for
    /// exactly one page: re-baseline from the next accepted page
    /// instead of reporting cascading gaps.
    tainted: bool,
}

/// Borrowed view of one structurally accepted page.
struct RawPage<'a> {
    offset: u64,
    version: u8,
    flags: u8,
    granule: i64,
    serial: u32,
    seq_no: u32,
    lacing: &'a [u8],
}

impl RawPage<'_> {
    fn is_continued(&self) -> bool {
        self.flags & flags::CONTINUED != 0
    }
    fn is_bos(&self) -> bool {
        self.flags & flags::FIRST_PAGE != 0
    }
    fn is_eos(&self) -> bool {
        self.flags & flags::LAST_PAGE != 0
    }
    /// Number of packets that finish on this page (lacing values < 255).
    fn completed_packets(&self) -> usize {
        self.lacing.iter().filter(|&&l| l < 255).count()
    }
    /// True when the page ends on a 255 lacing value (a packet stays
    /// open across the page boundary).
    fn ends_open(&self) -> bool {
        self.lacing.last().is_some_and(|&l| l == 255)
    }
}

/// Try to read one page's header at `pos`; classifies failures.
enum PageAt<'a> {
    /// A structurally complete page with matching CRC; `next` is the
    /// offset just past it.
    Ok { page: RawPage<'a>, next: usize },
    /// Capture pattern missing at `pos`.
    NoCapture,
    /// Capture pattern present but the buffer ends inside the page.
    Truncated { need: usize, have: usize },
    /// Structurally complete but the checksum does not match; the header
    /// fields are untrusted (claimed values reported for diagnostics).
    BadCrc {
        claimed_serial: u32,
        computed: u32,
        stored: u32,
        len: usize,
    },
}

fn page_at(data: &[u8], pos: usize) -> PageAt<'_> {
    let rest = &data[pos..];
    if rest.len() < 4 || rest[0..4] != *b"OggS" {
        return PageAt::NoCapture;
    }
    if rest.len() < 27 {
        return PageAt::Truncated {
            need: 27,
            have: rest.len(),
        };
    }
    let n_segs = rest[26] as usize;
    let header_len = 27 + n_segs;
    if rest.len() < header_len {
        return PageAt::Truncated {
            need: header_len,
            have: rest.len(),
        };
    }
    let lacing = &rest[27..header_len];
    let body_len: usize = lacing.iter().map(|&v| v as usize).sum();
    let total = header_len + body_len;
    if rest.len() < total {
        return PageAt::Truncated {
            need: total,
            have: rest.len(),
        };
    }
    let stored = u32::from_le_bytes(rest[22..26].try_into().expect("4 bytes"));
    let computed = crc::compute_page_checksum(&rest[..total]).expect("page slice >= 27 bytes");
    if computed != stored {
        return PageAt::BadCrc {
            claimed_serial: u32::from_le_bytes(rest[14..18].try_into().expect("4 bytes")),
            computed,
            stored,
            len: total,
        };
    }
    PageAt::Ok {
        page: RawPage {
            offset: pos as u64,
            version: rest[4],
            flags: rest[5],
            granule: i64::from_le_bytes(rest[6..14].try_into().expect("8 bytes")),
            serial: u32::from_le_bytes(rest[14..18].try_into().expect("4 bytes")),
            seq_no: u32::from_le_bytes(rest[18..22].try_into().expect("4 bytes")),
            lacing,
        },
        next: pos + total,
    }
}

/// Scan forward from `from` for the next offset holding a structurally
/// complete, checksum-valid page (RFC 3533 §6 field 1: "Once the capture
/// pattern is found, the decoder verifies page sync and integrity by
/// computing and comparing the checksum"). Returns `None` when no such
/// page remains.
fn find_next_valid_page(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 4 <= data.len() {
        if data[i..i + 4] != *b"OggS" {
            i += 1;
            continue;
        }
        match page_at(data, i) {
            PageAt::Ok { .. } => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Walk a complete physical Ogg bitstream and report every RFC 3533
/// page-structure violation. See the [module documentation](self) for
/// the rule set.
///
/// Never panics, regardless of input; memory use is bounded by
/// [`MAX_ISSUES`] (no page body is copied).
#[must_use]
pub fn validate(data: &[u8]) -> ConformanceReport {
    use std::collections::HashMap;

    let mut report = ConformanceReport::default();
    // Serial → model for bitstreams of the current link.
    let mut live: HashMap<u32, StreamModel> = HashMap::new();
    // Serials whose bitstream ended in a previous link (or earlier in
    // this one) — a later BOS reusing one violates §4 serial uniqueness.
    let mut finished: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // Whether any non-BOS page has been accepted in the current chain
    // link (the link's "data section" has begun).
    let mut link_has_data = false;
    let mut any_page = false;

    let mut pos = 0usize;
    while pos < data.len() {
        match page_at(data, pos) {
            PageAt::NoCapture => {
                // §6 field 1: junk between pages. Report the whole span
                // up to the next checksum-valid page as one issue.
                let next = find_next_valid_page(data, pos + 1);
                let end = next.unwrap_or(data.len());
                let skipped = (end - pos) as u64;
                report.junk_bytes += skipped;
                report.push(Issue {
                    rule: Rule::CapturePattern,
                    severity: Rule::CapturePattern.severity(),
                    byte_offset: pos as u64,
                    page_index: None,
                    serial: None,
                    detail: format!(
                        "{skipped} byte(s) without an 'OggS' capture pattern before {}",
                        if next.is_some() {
                            "the next valid page"
                        } else {
                            "end of input"
                        }
                    ),
                });
                match next {
                    Some(n) => pos = n,
                    None => break,
                }
            }
            PageAt::Truncated { need, have } => {
                report.push(Issue {
                    rule: Rule::Truncated,
                    severity: Rule::Truncated.severity(),
                    byte_offset: pos as u64,
                    page_index: None,
                    serial: None,
                    detail: format!(
                        "input ends inside a page: header declares {need} byte(s), {have} remain"
                    ),
                });
                // A truncated tail may still hide a later intact page
                // (e.g. junk that happens to start with 'OggS' spliced
                // ahead of real pages); rescan rather than abort.
                match find_next_valid_page(data, pos + 1) {
                    Some(n) => pos = n,
                    None => break,
                }
            }
            PageAt::BadCrc {
                claimed_serial,
                computed,
                stored,
                len,
            } => {
                report.push(Issue {
                    rule: Rule::CrcMismatch,
                    severity: Rule::CrcMismatch.severity(),
                    byte_offset: pos as u64,
                    page_index: None,
                    serial: Some(claimed_serial),
                    detail: format!(
                        "page checksum {stored:#010x} does not match computed {computed:#010x} \
                         over {len} byte(s)"
                    ),
                });
                // The damaged page's fields are untrusted; re-baseline
                // the claimed bitstream's model at its next page instead
                // of reporting a cascade of sequence/continuity issues
                // this CRC failure already explains.
                if let Some(model) = live.get_mut(&claimed_serial) {
                    model.tainted = true;
                }
                match find_next_valid_page(data, pos + 1) {
                    Some(n) => pos = n,
                    None => break,
                }
            }
            PageAt::Ok { page, next } => {
                if page.version != 0 {
                    // §6 field 2: only version 0 is specified. The page
                    // layout of another version is unknown; skip the
                    // page as opaque (its extent was computed with the
                    // version-0 layout, the only recovery available).
                    report.push(Issue {
                        rule: Rule::Version,
                        severity: Rule::Version.severity(),
                        byte_offset: page.offset,
                        page_index: Some(report.pages),
                        serial: Some(page.serial),
                        detail: format!(
                            "stream_structure_version {} (only 0 is specified)",
                            page.version
                        ),
                    });
                    if let Some(model) = live.get_mut(&page.serial) {
                        model.tainted = true;
                    }
                    pos = next;
                    continue;
                }
                if !any_page {
                    any_page = true;
                    report.links = 1;
                }
                let page_index = report.pages;
                report.pages += 1;
                check_page(
                    &page,
                    page_index,
                    &mut report,
                    &mut live,
                    &mut finished,
                    &mut link_has_data,
                );
                pos = next;
            }
        }
    }

    if !any_page && data.is_empty() {
        report.push(Issue {
            rule: Rule::Truncated,
            severity: Rule::Truncated.severity(),
            byte_offset: 0,
            page_index: None,
            serial: None,
            detail: "empty input: a physical bitstream carries at least one page".into(),
        });
    }

    // End of file: §4 — every logical bitstream "ends with a special
    // page (eos)".
    let mut open: Vec<u32> = live
        .iter()
        .filter(|(_, m)| !m.eos)
        .map(|(&s, _)| s)
        .collect();
    open.sort_unstable();
    for serial in open {
        report.push(Issue {
            rule: Rule::MissingEos,
            severity: Rule::MissingEos.severity(),
            byte_offset: data.len() as u64,
            page_index: None,
            serial: Some(serial),
            detail: "logical bitstream was never closed with an EOS page".into(),
        });
    }
    report
}

/// Apply the per-page rule checks and update the stream models.
fn check_page(
    page: &RawPage<'_>,
    page_index: u64,
    report: &mut ConformanceReport,
    live: &mut std::collections::HashMap<u32, StreamModel>,
    finished: &mut std::collections::HashSet<u32>,
    link_has_data: &mut bool,
) {
    let issue = |report: &mut ConformanceReport, rule: Rule, detail: String| {
        report.push(Issue {
            rule,
            severity: rule.severity(),
            byte_offset: page.offset,
            page_index: Some(page_index),
            serial: Some(page.serial),
            detail,
        });
    };

    if page.is_bos() {
        // §6 field 3: a BOS page starts a bitstream's first packet; it
        // cannot continue one.
        if page.is_continued() {
            issue(
                report,
                Rule::ContinuedBos,
                "BOS page carries the continued-packet flag".into(),
            );
        }
        // §6 field 6 convention: sequences start at 0.
        if page.seq_no != 0 {
            issue(
                report,
                Rule::SequenceStart,
                format!(
                    "first page of the bitstream has sequence number {}",
                    page.seq_no
                ),
            );
        }

        let duplicate = live.contains_key(&page.serial) && !live[&page.serial].eos;
        if duplicate {
            // §4: one BOS per logical bitstream; a second BOS on a live
            // serial means two bitstreams share it.
            issue(
                report,
                Rule::DuplicateBos,
                "second BOS page for a serial that is still live".into(),
            );
            // Model the restart: the new occupant's state replaces the
            // old (mirrors the demuxer's recovery).
        } else {
            // A BOS for a serial that ended earlier: chained links MUST
            // NOT reuse serials (§4).
            let ended_here = live.get(&page.serial).is_some_and(|m| m.eos);
            if finished.contains(&page.serial) || ended_here {
                issue(
                    report,
                    Rule::SerialReuse,
                    "BOS page reuses the serial of a logical bitstream that already ended".into(),
                );
            }
            // Link accounting: a BOS after the current link's data pages
            // began is either the start of the next chain link (legal
            // when every bitstream of the current link has ended) or a
            // grouping violation (§4: "all bos pages ... MUST appear
            // together at the beginning").
            if *link_has_data {
                let all_closed = live.values().all(|m| m.eos);
                if all_closed {
                    // New chain link: retire the previous link's models.
                    for (s, _) in live.drain() {
                        finished.insert(s);
                    }
                    *link_has_data = false;
                    report.links += 1;
                } else {
                    issue(
                        report,
                        Rule::BosNotContiguous,
                        "new bitstream's BOS page arrived after the link's data pages began, \
                         while other bitstreams of the link are still open"
                            .into(),
                    );
                }
            }
        }
        report.streams = report.streams.saturating_add(1);
        live.insert(
            page.serial,
            StreamModel {
                last_seq: page.seq_no,
                last_granule: None,
                open_packet: false,
                eos: false,
                after_eos_reported: false,
                tainted: false,
            },
        );
        let model = live.get_mut(&page.serial).expect("just inserted");
        finish_page_checks(page, model, report, page_index);
        return;
    }

    // Non-BOS page: the link's data section has begun.
    *link_has_data = true;

    if let std::collections::hash_map::Entry::Vacant(vacant) = live.entry(page.serial) {
        // §4: every logical bitstream starts with a BOS page. Report
        // once, then model the orphan bitstream so its subsequent pages
        // are still checked (and don't re-report per page).
        issue(
            report,
            Rule::MissingBos,
            "data page for a serial that never had a BOS page".into(),
        );
        report.streams = report.streams.saturating_add(1);
        let model = vacant.insert(StreamModel {
            last_seq: page.seq_no,
            last_granule: None,
            open_packet: false,
            eos: false,
            after_eos_reported: false,
            tainted: false,
        });
        finish_page_checks(page, model, report, page_index);
        return;
    }

    let model = live.get_mut(&page.serial).expect("presence checked");

    if model.eos {
        // §4: the EOS page is the final page of a logical bitstream.
        if !model.after_eos_reported {
            model.after_eos_reported = true;
            issue(
                report,
                Rule::PageAfterEos,
                "page for a logical bitstream after its EOS page".into(),
            );
        }
        // Keep re-baselining so a stray tail doesn't spam further rules.
        model.last_seq = page.seq_no;
        model.open_packet = page.ends_open();
        return;
    }

    if model.tainted {
        // The previous page claiming this serial was skipped (CRC or
        // version damage): sequence/continuity/granule state is
        // unknowable for one page. Re-baseline silently.
        model.tainted = false;
        model.last_seq = page.seq_no;
        if page.completed_packets() > 0 && page.granule != -1 {
            model.last_granule = Some(page.granule);
        }
        model.open_packet = page.ends_open();
        if page.is_eos() {
            model.eos = true;
        }
        return;
    }

    // §6 field 6: the only legal successor of seq N is N+1.
    let expected = model.last_seq.wrapping_add(1);
    let mut hole = false;
    if page.seq_no != expected {
        hole = true;
        let (rule, verb) = if page.seq_no.wrapping_sub(model.last_seq) as i32 > 0 {
            (Rule::SequenceGap, "jumped")
        } else {
            (Rule::SequenceRegression, "regressed")
        };
        issue(
            report,
            rule,
            format!(
                "page_sequence_number {} {} (expected {})",
                page.seq_no, verb, expected
            ),
        );
    }

    // §6 field 3: continued flag vs the previous page's lacing. Skipped
    // when a sequence discontinuity was just reported — the missing
    // pages already explain any mismatch.
    if !hole {
        if page.is_continued() && !model.open_packet {
            issue(
                report,
                Rule::ContinuedWithoutPartial,
                "continued flag set but the previous page ended with a completed packet".into(),
            );
        } else if !page.is_continued() && model.open_packet {
            issue(
                report,
                Rule::PartialNotContinued,
                "previous page ended mid-packet but the continued flag is unset".into(),
            );
        }
    }

    // §6 field 4: granule semantics (checked in the shared tail).
    model.last_seq = page.seq_no;
    if hole {
        // Reassembly state across the gap is unknowable; re-baseline.
        model.open_packet = page.ends_open();
        model.last_granule = match (page.completed_packets(), page.granule) {
            (0, _) | (_, -1) => model.last_granule,
            (_, g) => Some(g),
        };
        if page.is_eos() {
            model.eos = true;
        }
        return;
    }
    finish_page_checks(page, model, report, page_index);
}

/// Granule-position and EOS checks shared by every accepted page, plus
/// the model update.
fn finish_page_checks(
    page: &RawPage<'_>,
    model: &mut StreamModel,
    report: &mut ConformanceReport,
    page_index: u64,
) {
    let issue = |report: &mut ConformanceReport, rule: Rule, detail: String| {
        report.push(Issue {
            rule,
            severity: rule.severity(),
            byte_offset: page.offset,
            page_index: Some(page_index),
            serial: Some(page.serial),
            detail,
        });
    };

    let completed = page.completed_packets();
    if !page.lacing.is_empty() {
        if completed == 0 && page.granule != -1 {
            // §6 field 4: -1 is reserved for exactly this case.
            issue(
                report,
                Rule::SpuriousGranule,
                format!(
                    "granule_position {} on a page where no packet finishes (must be -1)",
                    page.granule
                ),
            );
        } else if completed > 0 && page.granule == -1 {
            issue(
                report,
                Rule::MissingGranule,
                format!("granule_position -1 on a page where {completed} packet(s) finish"),
            );
        }
    }
    if completed > 0 && page.granule != -1 {
        if let Some(prev) = model.last_granule {
            if page.granule < prev {
                issue(
                    report,
                    Rule::GranuleRegression,
                    format!(
                        "granule_position {} below the bitstream's previous {}",
                        page.granule, prev
                    ),
                );
            }
        }
        model.last_granule = Some(page.granule);
    }

    model.open_packet = page.ends_open();
    if page.is_eos() {
        model.eos = true;
        if model.open_packet {
            // §5: the packet this page leaves open can never complete.
            issue(
                report,
                Rule::EosMidPacket,
                "EOS page ends on a 255 lacing value, leaving a packet open forever".into(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::PageWriter;
    use crate::page::Page;

    /// Serialize one hand-built page (valid CRC).
    fn raw(serial: u32, seq: u32, fl: u8, granule: i64, lacing: Vec<u8>) -> Vec<u8> {
        let body_len: usize = lacing.iter().map(|&v| v as usize).sum();
        Page {
            flags: fl,
            granule_position: granule,
            serial,
            seq_no: seq,
            lacing,
            data: vec![0xA5; body_len],
        }
        .to_bytes()
    }

    /// A clean three-page single bitstream: BOS, data, EOS.
    fn clean_stream(serial: u32) -> Vec<u8> {
        let mut w = PageWriter::new(serial);
        w.push_packet(&[1u8; 40], 0);
        w.flush_page();
        w.push_packet(&[2u8; 300], 100);
        w.flush_page();
        w.push_packet(&[3u8; 25], 200);
        w.finish()
    }

    fn rules(report: &ConformanceReport) -> Vec<Rule> {
        report.issues.iter().map(|i| i.rule).collect()
    }

    #[test]
    fn clean_single_stream_is_clean() {
        let bytes = clean_stream(0x11);
        let r = validate(&bytes);
        assert!(r.is_clean(), "unexpected issues:\n{r}");
        assert_eq!(r.pages, 3);
        assert_eq!(r.streams, 1);
        assert_eq!(r.links, 1);
        assert_eq!(r.junk_bytes, 0);
    }

    #[test]
    fn empty_input_reports_truncated() {
        let r = validate(&[]);
        assert_eq!(rules(&r), vec![Rule::Truncated]);
        assert_eq!(r.pages, 0);
        assert_eq!(r.links, 0);
    }

    #[test]
    fn leading_and_trailing_junk_are_one_issue_each() {
        let stream = clean_stream(1);
        let mut bytes = b"garbage!".to_vec();
        bytes.extend_from_slice(&stream);
        bytes.extend_from_slice(b"tail-junk");
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::CapturePattern, Rule::CapturePattern]);
        assert_eq!(r.junk_bytes, 8 + 9);
        assert_eq!(r.pages, 3, "pages after the junk must still be walked");
        assert_eq!(r.issues[0].byte_offset, 0);
        assert_eq!(r.issues[1].byte_offset, (8 + stream.len()) as u64);
    }

    #[test]
    fn crc_damage_reports_once_and_rebaselines() {
        // Damage the middle page's body: one CrcMismatch, no cascading
        // sequence/continuity issues on the following page.
        let bytes = clean_stream(2);
        let p0 = Page::parse(&bytes).unwrap().1;
        let p1 = Page::parse(&bytes[p0..]).unwrap().1;
        let mut damaged = bytes.clone();
        let mid_body = p0 + p1 - 1;
        damaged[mid_body] ^= 0xFF;
        let r = validate(&damaged);
        assert_eq!(rules(&r), vec![Rule::CrcMismatch]);
        assert_eq!(r.issues[0].serial, Some(2));
        assert_eq!(r.pages, 2, "the damaged page is not accepted");
    }

    #[test]
    fn truncated_final_page_is_reported() {
        let bytes = clean_stream(3);
        let cut = &bytes[..bytes.len() - 5];
        let r = validate(cut);
        assert!(r.has(Rule::Truncated), "issues:\n{r}");
        // The EOS page was lost to the cut, so the bitstream is open.
        assert!(r.has(Rule::MissingEos));
    }

    #[test]
    fn version_nonzero_is_reported_when_crc_is_recomputed() {
        let mut bytes = raw(4, 0, flags::FIRST_PAGE | flags::LAST_PAGE, 10, vec![20]);
        bytes[4] = 1; // stream_structure_version
        let crc = crc::compute_page_checksum(&bytes).unwrap();
        bytes[crc::CRC_FIELD_OFFSET..crc::CRC_FIELD_OFFSET + crc::CRC_FIELD_LEN]
            .copy_from_slice(&crc.to_le_bytes());
        let r = validate(&bytes);
        assert!(r.has(Rule::Version), "issues:\n{r}");
    }

    #[test]
    fn missing_bos_and_missing_eos() {
        // A lone data page: no BOS, never closed.
        let bytes = raw(5, 3, 0, 50, vec![10]);
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::MissingBos, Rule::MissingEos]);
        assert_eq!(r.streams, 1);
    }

    #[test]
    fn duplicate_bos_on_live_serial() {
        let mut bytes = raw(6, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(6, 0, flags::FIRST_PAGE | flags::LAST_PAGE, 5, vec![7]));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::DuplicateBos]);
    }

    #[test]
    fn serial_reuse_across_links() {
        let mut bytes = clean_stream(7);
        bytes.extend_from_slice(&clean_stream(7));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::SerialReuse]);
        assert_eq!(r.links, 2, "the reuse still forms a second link");
    }

    #[test]
    fn valid_chain_counts_links_and_stays_clean() {
        let mut bytes = clean_stream(8);
        bytes.extend_from_slice(&clean_stream(9));
        bytes.extend_from_slice(&clean_stream(10));
        let r = validate(&bytes);
        assert!(r.is_clean(), "issues:\n{r}");
        assert_eq!(r.links, 3);
        assert_eq!(r.streams, 3);
    }

    #[test]
    fn grouped_streams_interleaved_stay_clean() {
        // BOS A, BOS B, data A, data B, EOS A, EOS B.
        let mut bytes = raw(0xA, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(0xB, 0, flags::FIRST_PAGE, 0, vec![7]));
        bytes.extend_from_slice(&raw(0xA, 1, 0, 10, vec![9]));
        bytes.extend_from_slice(&raw(0xB, 1, 0, 10, vec![9]));
        bytes.extend_from_slice(&raw(0xA, 2, flags::LAST_PAGE, 20, vec![3]));
        bytes.extend_from_slice(&raw(0xB, 2, flags::LAST_PAGE, 20, vec![3]));
        let r = validate(&bytes);
        assert!(r.is_clean(), "issues:\n{r}");
        assert_eq!(r.streams, 2);
        assert_eq!(r.links, 1);
    }

    #[test]
    fn late_bos_while_link_open_is_not_contiguous() {
        let mut bytes = raw(0xA, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(0xA, 1, 0, 10, vec![9])); // data begins
        bytes.extend_from_slice(&raw(0xB, 0, flags::FIRST_PAGE, 0, vec![7])); // late BOS
        bytes.extend_from_slice(&raw(0xA, 2, flags::LAST_PAGE, 20, vec![3]));
        bytes.extend_from_slice(&raw(0xB, 1, flags::LAST_PAGE, 5, vec![3]));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::BosNotContiguous]);
    }

    #[test]
    fn page_after_eos_is_reported_once() {
        let mut bytes = raw(0xC, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(0xC, 1, flags::LAST_PAGE, 10, vec![9]));
        bytes.extend_from_slice(&raw(0xC, 2, 0, 20, vec![9]));
        bytes.extend_from_slice(&raw(0xC, 3, 0, 30, vec![9]));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::PageAfterEos]);
    }

    #[test]
    fn continued_bos_is_reported() {
        let bytes = raw(
            0xD,
            0,
            flags::FIRST_PAGE | flags::CONTINUED | flags::LAST_PAGE,
            0,
            vec![7],
        );
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::ContinuedBos]);
    }

    #[test]
    fn nonzero_sequence_start_is_a_warning() {
        let bytes = raw(0xE, 4, flags::FIRST_PAGE | flags::LAST_PAGE, 0, vec![7]);
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::SequenceStart]);
        assert_eq!(r.issues[0].severity, Severity::Warning);
        assert_eq!(r.error_count(), 0);
        assert_eq!(r.warning_count(), 1);
    }

    #[test]
    fn sequence_gap_and_regression() {
        let mut bytes = raw(0xF, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(0xF, 5, 0, 10, vec![9])); // jump 1→5
        bytes.extend_from_slice(&raw(0xF, 2, flags::LAST_PAGE, 20, vec![3])); // back to 2
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::SequenceGap, Rule::SequenceRegression]);
    }

    #[test]
    fn granule_regression_is_reported() {
        let mut bytes = raw(0x10, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(0x10, 1, 0, 100, vec![9]));
        bytes.extend_from_slice(&raw(0x10, 2, flags::LAST_PAGE, 50, vec![3]));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::GranuleRegression]);
    }

    #[test]
    fn spurious_granule_on_packetless_page() {
        // All-255 lacing: no packet finishes, so the granule must be -1.
        let mut bytes = raw(0x11, 0, flags::FIRST_PAGE, 77, vec![255, 255]);
        bytes.extend_from_slice(&raw(
            0x11,
            1,
            flags::CONTINUED | flags::LAST_PAGE,
            99,
            vec![4],
        ));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::SpuriousGranule]);
    }

    #[test]
    fn missing_granule_on_completing_page_is_a_warning() {
        let mut bytes = raw(0x12, 0, flags::FIRST_PAGE, -1, vec![7]);
        bytes.extend_from_slice(&raw(0x12, 1, flags::LAST_PAGE, 10, vec![3]));
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::MissingGranule]);
        assert_eq!(r.issues[0].severity, Severity::Warning);
    }

    #[test]
    fn continued_flag_mismatches() {
        // Page 1 claims continuation after a completed packet.
        let mut a = raw(0x13, 0, flags::FIRST_PAGE, 0, vec![7]);
        a.extend_from_slice(&raw(
            0x13,
            1,
            flags::CONTINUED | flags::LAST_PAGE,
            10,
            vec![3],
        ));
        let ra = validate(&a);
        assert_eq!(rules(&ra), vec![Rule::ContinuedWithoutPartial]);

        // Page 0 ends open (255 lacing) but page 1 claims a fresh packet.
        let mut b = raw(0x14, 0, flags::FIRST_PAGE, -1, vec![255]);
        b.extend_from_slice(&raw(0x14, 1, flags::LAST_PAGE, 10, vec![3]));
        let rb = validate(&b);
        assert_eq!(rules(&rb), vec![Rule::PartialNotContinued]);
    }

    #[test]
    fn eos_mid_packet_is_reported() {
        let bytes = raw(0x15, 0, flags::FIRST_PAGE | flags::LAST_PAGE, -1, vec![255]);
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::EosMidPacket]);
    }

    #[test]
    fn hole_suppresses_continuity_noise() {
        // A sequence gap re-baselines reassembly: the next page's
        // continued flag is not double-reported.
        let mut bytes = raw(0x16, 0, flags::FIRST_PAGE, -1, vec![255]); // ends open
        bytes.extend_from_slice(&raw(0x16, 7, flags::LAST_PAGE, 10, vec![3])); // gap, fresh
        let r = validate(&bytes);
        assert_eq!(rules(&r), vec![Rule::SequenceGap]);
    }

    #[test]
    fn issue_cap_counts_overflow() {
        // 1100 packetless pages each with a spurious granule, all one
        // open mega-packet: 1100 SpuriousGranule issues, capped.
        let mut bytes = raw(0x17, 0, flags::FIRST_PAGE, 3, vec![255]);
        for seq in 1..1100u32 {
            bytes.extend_from_slice(&raw(0x17, seq, flags::CONTINUED, 3, vec![255]));
        }
        let r = validate(&bytes);
        assert_eq!(r.issues.len(), MAX_ISSUES);
        assert!(r.suppressed_issues > 0, "cap must count, not drop silently");
        assert!(!r.is_clean());
    }

    #[test]
    fn nil_eos_page_is_clean() {
        // RFC 3533 §4: "Eos pages may be 'nil' pages, that is, pages
        // containing no content but simply a page header with position
        // information and the eos flag set."
        let mut bytes = raw(0x18, 0, flags::FIRST_PAGE, 0, vec![7]);
        bytes.extend_from_slice(&raw(0x18, 1, flags::LAST_PAGE, 10, Vec::new()));
        let r = validate(&bytes);
        assert!(r.is_clean(), "issues:\n{r}");
    }

    #[test]
    fn report_display_is_stable() {
        let bytes = raw(0x19, 0, flags::FIRST_PAGE, 0, vec![7]);
        let r = validate(&bytes);
        let text = format!("{r}");
        assert!(text.contains("1 pages"), "display: {text}");
        assert!(text.contains("missing-eos"), "display: {text}");
    }
}
