//! Ogg demuxer: page reader → per-stream packet reassembly.

use std::collections::HashMap;
use std::io::Read;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Error, MediaType, Packet, Result, StreamInfo, TimeBase,
};
use oxideav_core::{Demuxer, ReadSeek};

use crate::codec_id;
use crate::page::{self, Page};

/// Open an Ogg bitstream.
pub fn open(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut state = OggDemuxer::new(input);
    state.read_bos_section()?;
    state.read_until_headers_collected()?;
    state.populate_extradata();
    state.populate_metadata();
    state.populate_duration();
    Ok(Box::new(state))
}

/// Open an Ogg bitstream and pre-build the page-level seek index by
/// scanning every page header in the file once. The index makes
/// subsequent [`Demuxer::seek_to`] calls O(log n) lookup + a single seek
/// instead of a log-n bisection that re-reads the file each time. Pages
/// with granule `-1` (no packet boundary) are skipped per RFC 3533 §6.
///
/// Returns the same boxed [`Demuxer`] as [`open`]; the index lives
/// inside the concrete type and accelerates seek_to transparently.
pub fn open_indexed(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut state = open_concrete(input, codecs)?;
    state.build_seek_index()?;
    Ok(Box::new(state))
}

/// Open an Ogg bitstream and return the concrete [`OggDemuxer`] type
/// (rather than a boxed trait object). Useful for callers that want to
/// invoke [`OggDemuxer::build_seek_index`] / [`OggDemuxer::seek_index_len`]
/// on demand without going through trait downcasts.
pub fn open_concrete(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<OggDemuxer> {
    let mut state = OggDemuxer::new(input);
    state.read_bos_section()?;
    state.read_until_headers_collected()?;
    state.populate_extradata();
    state.populate_metadata();
    state.populate_duration();
    Ok(state)
}

struct LogicalStream {
    /// Index into the public `streams` vec.
    public_index: usize,
    /// Buffered partial-packet bytes from a previous page that ended without
    /// a terminator (lacing 255). Concatenated with the next page's leading
    /// segments to form a complete packet.
    pending: Vec<u8>,
    /// Number of header packets still to be absorbed (not delivered).
    headers_remaining: usize,
    /// Header packets accumulated so far — used to populate codec-specific
    /// extradata on the stream's `CodecParameters` once they're all in.
    header_packets: Vec<Vec<u8>>,
    granule_seen: i64,
    /// Chained-link index this stream belongs to. RFC 3533 §4: chained Ogg
    /// is the concatenation of independent logical bitstreams; the first
    /// link is index 0, each subsequent BOS-after-non-BOS group increments
    /// the counter. Streams sharing a link play concurrently (multiplex);
    /// streams in different links play sequentially.
    link_index: u32,
    /// `page_sequence_number` of the last page consumed for this stream, or
    /// `None` before the first page. RFC 3533 §6 field 6: this counter
    /// "is increasing on each logical bitstream separately" and exists "so
    /// the decoder can identify page loss." A consumed page whose `seq_no`
    /// is not exactly `last_seq + 1` signals one or more dropped pages — a
    /// "hole" — which the demuxer must not paper over by silently splicing
    /// the broken packet halves together.
    last_seq: Option<u32>,
    /// True when the packet currently buffered in `pending` was left
    /// unterminated by a page that ended with a 255-lacing segment AND that
    /// page was not itself a continuation orphaned by a hole. Cleared once
    /// the packet completes or a hole invalidates the partial bytes.
    pending_valid: bool,
}

/// Concrete Ogg demuxer state. Most callers should use the boxed
/// [`Demuxer`] returned by [`open`] / [`open_indexed`]; this type is
/// public so consumers who want the inherent [`build_seek_index`] /
/// [`seek_index_len`] API can hold it directly.
pub struct OggDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    state_by_serial: HashMap<u32, LogicalStream>,
    /// Pages we've already read but not yet drained for packets.
    page_queue: std::collections::VecDeque<Page>,
    /// Packets ready to emit, in insertion order across all streams.
    out_queue: std::collections::VecDeque<Packet>,
    /// True once we've read past the BOS section and into the data pages.
    eof_reached: bool,
    metadata: Vec<(String, String)>,
    duration_micros: i64,
    /// Per-serial sorted page-level seek index. Each value is a list of
    /// `(granule, page_offset)` ordered by `granule`. Pages with granule
    /// `-1` (no packet terminates on the page) are NOT indexed because
    /// they carry no usable seek timestamp. Built lazily as pages flow
    /// through the demuxer, and may be densely pre-populated by
    /// [`OggDemuxer::build_seek_index`].
    seek_index: HashMap<u32, Vec<(i64, u64)>>,
    /// True once `build_seek_index` has run successfully, so we don't
    /// repeat the full-file scan on later calls.
    seek_index_built: bool,
    /// Next chained-link index to assign on the next BOS-after-non-BOS.
    /// Initialised to 0; the very first BOS section all shares link 0,
    /// and the counter increments on each subsequent BOS that follows
    /// at least one non-BOS page (the RFC 3533 §4 "link boundary").
    next_link_index: u32,
    /// True once we've seen any non-BOS page in the current link. Resets
    /// to false when a new link begins (so its multiplexed BOS pages all
    /// share the same link index).
    seen_nonbos_in_current_link: bool,
    /// Number of page-loss "holes" detected during demux. RFC 3533 §6
    /// field 6: a logical stream's `page_sequence_number` increases by one
    /// per page; a jump signals dropped pages. Each gap (regardless of how
    /// many pages went missing) counts as one hole. Surfaced via
    /// [`OggDemuxer::hole_count`] for diagnostics and tests.
    holes: u64,
}

impl OggDemuxer {
    fn new(input: Box<dyn ReadSeek>) -> Self {
        Self {
            input,
            streams: Vec::new(),
            state_by_serial: HashMap::new(),
            page_queue: std::collections::VecDeque::new(),
            out_queue: std::collections::VecDeque::new(),
            eof_reached: false,
            metadata: Vec::new(),
            duration_micros: 0,
            seek_index: HashMap::new(),
            seek_index_built: false,
            next_link_index: 0,
            seen_nonbos_in_current_link: false,
            holes: 0,
        }
    }

    /// Record a page's `(granule, byte_offset)` into the per-serial seek
    /// index, keeping each serial's list sorted by granule. Pages whose
    /// granule is `-1` (RFC 3533 §6: "no packets finish on this page")
    /// carry no seek-target information and are skipped. Duplicate
    /// inserts (same offset already present at the same granule) are
    /// suppressed so re-scans are idempotent.
    fn index_record(&mut self, serial: u32, granule: i64, offset: u64) {
        if granule < 0 {
            return;
        }
        let entries = self.seek_index.entry(serial).or_default();
        match entries.binary_search_by(|(g, o)| g.cmp(&granule).then_with(|| o.cmp(&offset))) {
            Ok(_) => {} // already present
            Err(pos) => entries.insert(pos, (granule, offset)),
        }
    }

    /// Look up the seek index for the largest entry on `serial` whose
    /// granule is `<= target`. Returns `(granule, page_offset)` if any
    /// such entry exists.
    fn index_floor(&self, serial: u32, target: i64) -> Option<(i64, u64)> {
        let entries = self.seek_index.get(&serial)?;
        if entries.is_empty() {
            return None;
        }
        // Find the rightmost entry with granule <= target.
        let idx = match entries.binary_search_by(|(g, _)| g.cmp(&target)) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        Some(entries[idx])
    }

    /// Walk every page header in the file once, recording
    /// `(serial, granule, page_offset)` into the seek index. Only the
    /// 27-byte page header + N-byte segment table are read; payload is
    /// skipped over with a single relative seek per page, so cost is
    /// O(pages) seeks, not O(bytes).
    ///
    /// After this returns, [`seek_to`] becomes O(log n) lookup + one
    /// seek for any covered timestamp on any logical stream. Pages
    /// whose granule is `-1` (no packet boundary, RFC 3533 §6) are
    /// skipped because they carry no seek-target information.
    ///
    /// On error the index is left partially populated — subsequent
    /// `seek_to` calls remain correct (they fall back to bisection for
    /// uncovered targets); only the speedup is incomplete.
    pub fn build_seek_index(&mut self) -> Result<()> {
        use std::io::SeekFrom;

        let saved_pos = self.input.stream_position()?;
        let end = self.input.seek(SeekFrom::End(0))?;
        if end == 0 {
            self.input.seek(SeekFrom::Start(saved_pos))?;
            self.seek_index_built = true;
            return Ok(());
        }

        // Scan from byte 0 — every Ogg page starts with `OggS`.
        let mut cursor: u64 = 0;
        // Re-use a chunk buffer to find `OggS` captures cheaply.
        const CHUNK: u64 = 64 * 1024;
        while cursor < end {
            self.input.seek(SeekFrom::Start(cursor))?;
            let want = CHUNK.min(end - cursor) as usize;
            let mut buf = vec![0u8; want];
            let mut filled = 0usize;
            while filled < buf.len() {
                match self.input.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            buf.truncate(filled);
            if buf.is_empty() {
                break;
            }

            // Find the first OggS capture in the chunk.
            let mut i = 0usize;
            let mut advanced = false;
            while i + 4 <= buf.len() {
                if &buf[i..i + 4] != b"OggS" {
                    i += 1;
                    continue;
                }
                let page_off = cursor + i as u64;

                // Re-seek to the candidate, read header + segment table.
                self.input.seek(SeekFrom::Start(page_off))?;
                let mut hdr = [0u8; 27];
                if self.input.read(&mut hdr)? != 27 {
                    // Truncated tail — we're done.
                    self.input.seek(SeekFrom::Start(saved_pos))?;
                    self.seek_index_built = true;
                    return Ok(());
                }
                if hdr[0..4] != page::CAPTURE_PATTERN {
                    // False positive inside some payload byte stream
                    // (rare, but possible). Skip past this byte and
                    // keep scanning.
                    i += 1;
                    continue;
                }
                let flags_byte = hdr[5];
                let granule = i64::from_le_bytes([
                    hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13],
                ]);
                let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
                let n_segs = hdr[26] as usize;
                let mut lacing = vec![0u8; n_segs];
                if self.input.read(&mut lacing)? != n_segs {
                    self.input.seek(SeekFrom::Start(saved_pos))?;
                    self.seek_index_built = true;
                    return Ok(());
                }
                let data_len: u64 = lacing.iter().map(|&v| v as u64).sum();
                self.index_record(serial, granule, page_off);

                // Chained-link discovery (RFC 3533 §4): a BOS page for an
                // unfamiliar serial encountered during the scan is the
                // start of a new logical bitstream. If we've already seen
                // any non-BOS page since the most recent link change, the
                // new BOS opens a new chained link; otherwise it joins
                // the current link's multiplex. Pre-registering the
                // stream here means a post-scan duration calculation can
                // attribute that link's last granule to a known time
                // base, even before any packet flows through the demuxer.
                let is_bos = flags_byte & page::flags::FIRST_PAGE != 0;
                if is_bos && !self.state_by_serial.contains_key(&serial) {
                    // Read the page payload so we can parse the first
                    // packet for codec_id + sample_rate. Identification
                    // packets must fit in a single page per the various
                    // codec mapping RFCs, so we only need this page's
                    // data, not any continuation.
                    let mut data = vec![0u8; data_len as usize];
                    if self.input.read(&mut data)? != data_len as usize {
                        self.input.seek(SeekFrom::Start(saved_pos))?;
                        self.seek_index_built = true;
                        return Ok(());
                    }
                    let synth = Page {
                        flags: flags_byte,
                        granule_position: granule,
                        serial,
                        seq_no: u32::from_le_bytes([hdr[18], hdr[19], hdr[20], hdr[21]]),
                        lacing: lacing.clone(),
                        data,
                    };
                    if self.seen_nonbos_in_current_link {
                        self.next_link_index = self.next_link_index.saturating_add(1);
                        self.seen_nonbos_in_current_link = false;
                    }
                    // Best-effort: ignore registration failure (a malformed
                    // BOS shouldn't abort the seek-index build).
                    let _ = self.register_stream(&synth);
                } else if !is_bos {
                    self.seen_nonbos_in_current_link = true;
                }

                // Jump cursor past the entire page body and resume
                // chunked OggS-hunting from there.
                cursor = page_off + 27 + n_segs as u64 + data_len;
                advanced = true;
                break;
            }
            if !advanced {
                // No OggS found in this chunk — advance past it (minus
                // a 3-byte tail to catch captures straddling the
                // boundary).
                if buf.len() < 4 {
                    break;
                }
                cursor += buf.len() as u64 - 3;
            }
        }

        self.input.seek(SeekFrom::Start(saved_pos))?;
        self.seek_index_built = true;
        // Now that every link's BOS has been registered and every page's
        // granule is indexed, recompute total duration as the sum of
        // per-link max granule (chained playback is sequential) rather
        // than the max-over-streams (which only works for a single
        // multiplexed link).
        self.populate_duration_from_index();
        Ok(())
    }

    /// Recompute `duration_micros` from the (fully populated) seek index.
    /// For chained Ogg files (multiple links), playback is sequential —
    /// total duration is the SUM of each link's duration, where each
    /// link's duration is the max-over-its-streams of last-granule
    /// translated through that stream's time_base. For non-chained
    /// (single-link) files, the result reduces to max-over-streams,
    /// matching `populate_duration`'s near-tail scan.
    fn populate_duration_from_index(&mut self) {
        // Group serials by link_index.
        let mut by_link: HashMap<u32, Vec<u32>> = HashMap::new();
        for (serial, state) in &self.state_by_serial {
            by_link.entry(state.link_index).or_default().push(*serial);
        }
        let mut total_micros: i64 = 0;
        for serials in by_link.values() {
            let mut link_micros: i64 = 0;
            for serial in serials {
                let Some(entries) = self.seek_index.get(serial) else {
                    continue;
                };
                let last_granule = entries.last().map(|(g, _)| *g).unwrap_or(0);
                let Some(state) = self.state_by_serial.get(serial) else {
                    continue;
                };
                let stream = &self.streams[state.public_index];
                let us = (stream.time_base.seconds_of(last_granule) * 1_000_000.0) as i64;
                if us > link_micros {
                    link_micros = us;
                }
            }
            total_micros = total_micros.saturating_add(link_micros);
        }
        if total_micros > 0 {
            self.duration_micros = total_micros;
        }
    }

    /// Total number of indexed pages across all logical streams. Useful
    /// for tests and external profiling. Excludes pages whose granule
    /// position is `-1`.
    pub fn seek_index_len(&self) -> usize {
        self.seek_index.values().map(|v| v.len()).sum()
    }

    /// Number of page-loss "holes" the demuxer has detected so far across
    /// all logical streams (RFC 3533 §6 field 6: a stream's
    /// `page_sequence_number` increments by one per page, so a jump in the
    /// counter signals dropped pages). Each gap counts once regardless of
    /// how many consecutive pages went missing. The count only reflects
    /// pages actually consumed via `next_packet` (or absorbed during the
    /// header phase) — `build_seek_index`'s header-only scan does not run
    /// packet reassembly and therefore does not contribute.
    ///
    /// Zero for a clean file. A non-zero value tells a caller the byte
    /// stream was truncated or corrupted between pages; any packet that
    /// spanned a hole was dropped rather than spliced from mismatched
    /// halves, so the surviving packets stay individually well-formed.
    pub fn hole_count(&self) -> u64 {
        self.holes
    }

    /// Read pages until we leave the Beginning-Of-Stream section, registering
    /// every logical bitstream we discover. The pages we read are queued so
    /// `next_packet` can drain them in order.
    fn read_bos_section(&mut self) -> Result<()> {
        loop {
            let page = match self.read_page()? {
                Some(p) => p,
                None => {
                    self.eof_reached = true;
                    break;
                }
            };
            let is_bos = page.is_first();
            if is_bos {
                self.register_stream(&page)?;
            }
            self.page_queue.push_back(page);
            if !is_bos {
                // The first non-BOS page marks the end of the BOS section.
                break;
            }
        }
        if self.streams.is_empty() {
            return Err(Error::invalid("Ogg file contains no logical streams"));
        }
        Ok(())
    }

    fn register_stream(&mut self, bos_page: &Page) -> Result<()> {
        // The BOS page's first packet is the identification packet for the
        // codec. Identification packets must fit in a single BOS page (RFC
        // 5334 / codec mapping conventions).
        let segs = bos_page.packet_segments();
        if segs.is_empty() {
            return Err(Error::invalid("Ogg BOS page has no packets"));
        }
        let first = &bos_page.data[segs[0].data.clone()];
        let codec_id = codec_id::detect(first);
        let public_index = self.streams.len();
        let mut params = guess_params(&codec_id, first)?;
        params.extradata = first.to_vec();

        let time_base = match codec_id.as_str() {
            "vorbis" | "flac" => {
                if let Some(sr) = params.sample_rate {
                    TimeBase::new(1, sr as i64)
                } else {
                    TimeBase::new(1, 1_000_000)
                }
            }
            // Opus uses a 48 kHz timebase regardless of input sample rate.
            "opus" => TimeBase::new(1, 48_000),
            _ => TimeBase::new(1, 1_000_000),
        };

        self.streams.push(StreamInfo {
            index: public_index as u32,
            time_base,
            duration: None,
            start_time: Some(0),
            params,
        });
        self.state_by_serial.insert(
            bos_page.serial,
            LogicalStream {
                public_index,
                pending: Vec::new(),
                headers_remaining: codec_id::header_packet_count(&codec_id),
                header_packets: Vec::new(),
                granule_seen: 0,
                link_index: self.next_link_index,
                last_seq: None,
                pending_valid: false,
            },
        );
        Ok(())
    }

    fn read_page(&mut self) -> Result<Option<Page>> {
        // Read a page header (27 bytes), then enough to read the segment table
        // and data. We detect EOF by getting 0 bytes back from the very first
        // read; partial-page data is treated as truncation.
        let page_off = self.input.stream_position().unwrap_or(0);
        let mut hdr = [0u8; 27];
        if !read_exact_or_eof(&mut self.input, &mut hdr)? {
            return Ok(None);
        }
        if hdr[0..4] != page::CAPTURE_PATTERN {
            return Err(Error::invalid("Ogg: lost page sync (no 'OggS')"));
        }
        let n_segs = hdr[26] as usize;
        let mut lacing = vec![0u8; n_segs];
        self.input.read_exact(&mut lacing)?;
        let data_len: usize = lacing.iter().map(|&v| v as usize).sum();
        let mut data = vec![0u8; data_len];
        self.input.read_exact(&mut data)?;

        // Re-parse from the assembled bytes so CRC validation logic is shared.
        let mut full = Vec::with_capacity(27 + n_segs + data_len);
        full.extend_from_slice(&hdr);
        full.extend_from_slice(&lacing);
        full.extend_from_slice(&data);
        let (page, consumed) = Page::parse(&full)?;
        debug_assert_eq!(consumed, full.len());
        // Opportunistically populate the seek index from any page we read
        // during normal demux flow — costs O(log n) per page and means a
        // subsequent seek can skip bisection if the target falls inside
        // the already-scanned range.
        self.index_record(page.serial, page.granule_position, page_off);
        Ok(Some(page))
    }

    /// After the BOS section, keep reading pages and absorbing header packets
    /// until every logical stream has gathered all of its expected setup
    /// packets (3 for Vorbis, 2 for Opus, …). Audio/video packets read in the
    /// process are still queued; they'll be delivered by `next_packet` later.
    fn read_until_headers_collected(&mut self) -> Result<()> {
        loop {
            let any_pending = self
                .state_by_serial
                .values()
                .any(|s| s.headers_remaining > 0);
            if !any_pending {
                return Ok(());
            }
            // Drain queued pages from the BOS phase first; only then read more.
            let page = if let Some(p) = self.page_queue.pop_front() {
                p
            } else {
                match self.read_page()? {
                    Some(p) => p,
                    None => return Ok(()), // EOF before all headers — best-effort.
                }
            };
            self.process_page(page)?;
        }
    }

    /// Build codec-specific extradata for each stream from its accumulated
    /// header packets and write it back to the stream's `CodecParameters`.
    fn populate_extradata(&mut self) {
        for state in self.state_by_serial.values() {
            let codec_id = self.streams[state.public_index].params.codec_id.clone();
            let extra = build_codec_private(&codec_id, &state.header_packets);
            if !extra.is_empty() {
                self.streams[state.public_index].params.extradata = extra;
            }
        }
    }

    /// Pull the Vorbis-comment block out of whichever stream carries it
    /// (Vorbis packet #2, Opus packet #2, Theora packet #2) and expose it
    /// as container metadata.
    fn populate_metadata(&mut self) {
        for state in self.state_by_serial.values() {
            let codec_id = self.streams[state.public_index].params.codec_id.clone();
            let packets = &state.header_packets;
            match codec_id.as_str() {
                "vorbis" if packets.len() >= 2 => {
                    // 2nd packet starts with 0x03 "vorbis" (7 bytes) then the comment body.
                    let p = &packets[1];
                    if p.len() > 7 && &p[1..7] == b"vorbis" {
                        parse_vorbis_comment(&p[7..], &mut self.metadata);
                    }
                }
                "opus" if packets.len() >= 2 => {
                    // 2nd packet is OpusTags: 8-byte "OpusTags" magic, then the comment body.
                    let p = &packets[1];
                    if p.len() > 8 && &p[..8] == b"OpusTags" {
                        parse_vorbis_comment(&p[8..], &mut self.metadata);
                    }
                }
                "theora" if packets.len() >= 2 => {
                    // 2nd packet: 0x81 "theora" (7 bytes) then comment body.
                    let p = &packets[1];
                    if p.len() > 7 && &p[1..7] == b"theora" {
                        parse_vorbis_comment(&p[7..], &mut self.metadata);
                    }
                }
                _ => {}
            }
        }
    }

    /// Seek to the end of the file and find the last page of the first
    /// audio-or-video stream to read its granule_position, which gives
    /// the total stream length in samples or video frames.
    fn populate_duration(&mut self) {
        use std::io::SeekFrom;
        let saved_pos = match self.input.stream_position() {
            Ok(p) => p,
            Err(_) => return,
        };
        let end = match self.input.seek(SeekFrom::End(0)) {
            Ok(e) => e,
            Err(_) => {
                let _ = self.input.seek(SeekFrom::Start(saved_pos));
                return;
            }
        };
        // Scan back up to 64 KB looking for the last 'OggS' capture pattern.
        let scan_back = end.min(64 * 1024);
        let start = end.saturating_sub(scan_back);
        if self.input.seek(SeekFrom::Start(start)).is_err() {
            return;
        }
        let mut buf = vec![0u8; scan_back as usize];
        if self.input.read_exact(&mut buf).is_err() {
            return;
        }
        // Find the rightmost OggS header and parse it.
        let mut last_granule_by_serial: HashMap<u32, i64> = HashMap::new();
        let mut i = 0usize;
        while i + 27 <= buf.len() {
            if &buf[i..i + 4] == b"OggS" && i + 27 + (buf[i + 26] as usize) <= buf.len() {
                let n_segs = buf[i + 26] as usize;
                let body_end_off = i + 27 + n_segs;
                let data_len: usize = buf[i + 27..body_end_off].iter().map(|&v| v as usize).sum();
                if body_end_off + data_len <= buf.len() {
                    let granule = i64::from_le_bytes([
                        buf[i + 6],
                        buf[i + 7],
                        buf[i + 8],
                        buf[i + 9],
                        buf[i + 10],
                        buf[i + 11],
                        buf[i + 12],
                        buf[i + 13],
                    ]);
                    let serial =
                        u32::from_le_bytes([buf[i + 14], buf[i + 15], buf[i + 16], buf[i + 17]]);
                    if granule >= 0 {
                        last_granule_by_serial.insert(serial, granule);
                    }
                    i = body_end_off + data_len;
                    continue;
                }
            }
            i += 1;
        }
        // Pick the longest duration across streams in their own time base.
        let mut best_micros = 0i64;
        for (serial, granule) in last_granule_by_serial {
            let Some(st) = self.state_by_serial.get(&serial) else {
                continue;
            };
            let stream = &self.streams[st.public_index];
            let us = (stream.time_base.seconds_of(granule) * 1_000_000.0) as i64;
            if us > best_micros {
                best_micros = us;
            }
        }
        self.duration_micros = best_micros;
        let _ = self.input.seek(SeekFrom::Start(saved_pos));
    }

    /// Starting at byte offset `start`, scan forward up to `limit` for the
    /// next Ogg page whose bitstream_serial_number equals `wanted_serial`.
    /// Returns `Some((page_offset, granule_position))` on success, or
    /// `None` if no such page is found in the window. Only the page header
    /// and segment table are read — the payload is skipped over by a
    /// single relative seek, so the cost is O(pages scanned), not O(bytes).
    fn find_next_page_for_serial(
        &mut self,
        start: u64,
        limit: u64,
        wanted_serial: u32,
    ) -> Result<Option<(u64, i64)>> {
        use std::io::SeekFrom;

        if start >= limit {
            return Ok(None);
        }

        // Read a chunk starting at `start` to find the next OggS capture.
        // Then for each candidate, seek to it, read its header + segment
        // table, and check the serial.
        let chunk_size: u64 = 64 * 1024;
        let mut cursor = start;
        while cursor < limit {
            let read_len = chunk_size.min(limit - cursor);
            self.input.seek(SeekFrom::Start(cursor))?;
            let mut buf = vec![0u8; read_len as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                match self.input.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
            buf.truncate(filled);
            if buf.is_empty() {
                return Ok(None);
            }

            // Find the next OggS capture in this buffer. We need at least
            // 27 bytes (header) to parse; if we find a match near the tail
            // we re-read from that offset to span the chunk boundary.
            let mut i = 0usize;
            while i + 4 <= buf.len() {
                if &buf[i..i + 4] != b"OggS" {
                    i += 1;
                    continue;
                }
                let page_off = cursor + i as u64;

                // Read the page header + segment table directly from the
                // input (cheaper than re-reading the whole data payload).
                self.input.seek(SeekFrom::Start(page_off))?;
                let mut hdr = [0u8; 27];
                if self.input.read(&mut hdr)? != 27 {
                    return Ok(None);
                }
                if hdr[0..4] != page::CAPTURE_PATTERN {
                    // False positive (byte alignment within some payload).
                    i += 1;
                    continue;
                }
                let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
                let granule = i64::from_le_bytes([
                    hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11], hdr[12], hdr[13],
                ]);
                let n_segs = hdr[26] as usize;

                if serial == wanted_serial && granule >= 0 {
                    self.index_record(serial, granule, page_off);
                    return Ok(Some((page_off, granule)));
                }

                // Even when the page isn't a match, record its
                // `(granule, page_off)` in the index — it costs nothing
                // and accelerates future seeks that DO target that serial.
                self.index_record(serial, granule, page_off);

                // Not our stream (or an "indeterminate" granule = -1).
                // Skip past this page's body to find the next one.
                let mut lacing = vec![0u8; n_segs];
                if self.input.read(&mut lacing)? != n_segs {
                    return Ok(None);
                }
                let data_len: u64 = lacing.iter().map(|&v| v as u64).sum();
                let next_off = page_off + 27 + n_segs as u64 + data_len;
                if next_off >= limit {
                    return Ok(None);
                }
                // Re-align our scanning cursor to the page end and
                // re-load the chunk buffer from there on the next pass.
                cursor = next_off;
                break;
            }
            // If we walked the whole buffer without finding OggS, advance
            // past it (minus 3 bytes to keep a possible capture that
            // straddles the boundary).
            if i + 4 > buf.len() {
                if buf.len() < 4 {
                    return Ok(None);
                }
                cursor += buf.len() as u64 - 3;
            }
        }
        Ok(None)
    }

    /// Drain the next packet from the queued pages, possibly reading more.
    fn drain_next(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.out_queue.pop_front() {
                return Ok(Some(p));
            }
            // Need to consume another page.
            let page = match self.page_queue.pop_front() {
                Some(p) => p,
                None => match self.read_page()? {
                    Some(p) => p,
                    None => {
                        self.eof_reached = true;
                        return Ok(None);
                    }
                },
            };
            self.process_page(page)?;
        }
    }

    fn process_page(&mut self, page: Page) -> Result<()> {
        // RFC 3533 §4 + Vorbis I §A.2: chained Ogg streams concatenate
        // independent logical bitstreams back-to-back, each with its own
        // BOS page. A BOS-flagged page for an unknown serial therefore
        // signals a NEW logical stream beginning mid-file — register it
        // before processing the page's packets so its identification
        // header is captured as a header packet, not delivered as data.
        //
        // For chained-link tracking: a BOS page that arrives AFTER any
        // non-BOS page in the current link starts a new link. BOS pages
        // arriving while `seen_nonbos_in_current_link` is still false
        // are part of the same link's BOS section (multiplex within one
        // link). Either way, the registered stream inherits the link
        // index that was current at the moment of its BOS.
        if page.is_first() && !self.state_by_serial.contains_key(&page.serial) {
            if self.seen_nonbos_in_current_link {
                self.next_link_index = self.next_link_index.saturating_add(1);
                self.seen_nonbos_in_current_link = false;
            }
            self.register_stream(&page)?;
        } else if !page.is_first() {
            self.seen_nonbos_in_current_link = true;
        }
        let Some(stream) = self.state_by_serial.get_mut(&page.serial) else {
            // Unknown serial that isn't a BOS — skip silently.
            return Ok(());
        };
        let public_index = stream.public_index;
        let stream_idx = self.streams[public_index].index;
        let time_base = self.streams[public_index].time_base;
        let segs = page.packet_segments();
        let was_continued = page.is_continued();

        // RFC 3533 §6 field 6: page_sequence_number "is increasing on each
        // logical bitstream separately" so "the decoder can identify page
        // loss." A non-BOS page whose seq_no isn't exactly last_seq+1 means
        // one or more pages were dropped between them — a "hole". The
        // counter is a wrapping u32, so the only legal successor of `prev`
        // is `prev.wrapping_add(1)`. A BOS page legitimately restarts the
        // counter (typically at 0) and is never treated as a hole.
        let hole = !page.is_first()
            && match stream.last_seq {
                Some(prev) => page.seq_no != prev.wrapping_add(1),
                None => false,
            };
        if hole {
            self.holes += 1;
            // Any packet bytes buffered from before the gap can never be
            // completed: the page(s) that carried the rest are gone. Drop
            // them so we don't splice unrelated halves into one packet.
            stream.pending.clear();
            stream.pending_valid = false;
        }

        // Collect every packet that terminates on this page; the page's
        // granule_position applies to the last such packet (per RFC 3533).
        // `pending_valid` tracks whether the bytes accumulating in `pending`
        // form the contiguous tail of a real packet (vs. an orphaned
        // continuation fragment whose head was lost to a hole).
        let mut completed: Vec<(Vec<u8>, bool)> = Vec::new();
        for (i, seg) in segs.iter().enumerate() {
            let payload = &page.data[seg.data.clone()];
            if i == 0 && was_continued {
                // Leading segment continues a packet from the previous page.
                // Only append if we actually hold that packet's valid head;
                // otherwise this fragment is the unrecoverable tail of a
                // packet orphaned by a page-loss hole and must be discarded.
                if stream.pending_valid {
                    stream.pending.extend_from_slice(payload);
                } else {
                    // Orphaned continuation: ensure the buffer is empty and
                    // leave it invalid so the fragment is dropped, not
                    // emitted, when it terminates.
                    stream.pending.clear();
                }
            } else {
                // A fresh packet starts here. Any leftover bytes in
                // `pending` at this point would be a partial packet the
                // previous page left unterminated AND that this page does
                // not continue (continuation bit unset, or this is not the
                // first segment) — that's a framing inconsistency, so drop
                // them defensively.
                stream.pending.clear();
                stream.pending.extend_from_slice(payload);
                stream.pending_valid = true;
            }
            if seg.terminated {
                let valid = stream.pending_valid;
                completed.push((std::mem::take(&mut stream.pending), valid));
                // The next packet (if any) on this page starts fresh.
                stream.pending_valid = false;
            }
        }
        // Invariant after the loop: if `pending` is non-empty it holds the
        // unterminated tail of a packet that began on this page (or extended
        // a valid one), so `pending_valid` is already true — the next page's
        // leading continuation segment may safely append to it. An orphaned
        // continuation fragment was cleared above, leaving `pending` empty
        // and `pending_valid` false, so its missing-head tail is never
        // mistaken for a resumable packet.
        // Drop completed packets that were assembled from orphaned
        // continuation fragments (head lost to a hole) — they're garbage.
        let completed: Vec<Vec<u8>> = completed
            .into_iter()
            .filter_map(|(data, valid)| if valid { Some(data) } else { None })
            .collect();

        let last_idx = completed.len().checked_sub(1);
        let mut headers_just_completed = false;
        for (i, data) in completed.into_iter().enumerate() {
            if stream.headers_remaining > 0 {
                stream.header_packets.push(data);
                stream.headers_remaining -= 1;
                if stream.headers_remaining == 0 {
                    headers_just_completed = true;
                }
                continue;
            }
            let is_last = Some(i) == last_idx;
            // pts on the last-on-page packet carries the page's granule
            // (Ogg's only timing signal); intermediate packets get None.
            // Container-aware muxers that need per-packet pts should derive
            // them from codec-specific knowledge (e.g. Opus TOC parsing).
            let pts = if is_last && page.granule_position >= 0 {
                Some(page.granule_position)
            } else {
                None
            };
            let mut pkt = Packet::new(stream_idx, time_base, data);
            pkt.pts = pts;
            pkt.dts = pts;
            pkt.flags.keyframe = true;
            pkt.flags.unit_boundary = is_last;
            self.out_queue.push_back(pkt);
        }

        // Track the most recently observed granule for debugging/analysis. Not
        // used to assign per-packet pts any more.
        if page.granule_position >= 0 {
            stream.granule_seen = page.granule_position;
        }
        // Record this page's sequence number so the next page on this stream
        // can be checked for continuity (RFC 3533 §6 field 6 page loss).
        stream.last_seq = Some(page.seq_no);

        // For chained streams whose headers complete after the initial
        // open() phase, rebuild extradata + metadata now so downstream
        // codec decoders see the same payload they would for a non-chained
        // stream. (`populate_extradata`/`populate_metadata` only run once
        // at open time and would otherwise leave the new stream with just
        // its identification packet in `extradata`.)
        if headers_just_completed {
            self.populate_extradata_for(public_index);
            self.populate_metadata_for(public_index);
        }
        Ok(())
    }

    /// Rebuild `params.extradata` for a single stream from its accumulated
    /// header packets. Used both during initial open and when a chained
    /// stream's headers complete mid-file.
    fn populate_extradata_for(&mut self, public_index: usize) {
        let codec_id = self.streams[public_index].params.codec_id.clone();
        let header_packets: Vec<Vec<u8>> = match self
            .state_by_serial
            .values()
            .find(|s| s.public_index == public_index)
        {
            Some(s) => s.header_packets.clone(),
            None => return,
        };
        let extra = build_codec_private(&codec_id, &header_packets);
        if !extra.is_empty() {
            self.streams[public_index].params.extradata = extra;
        }
    }

    /// Pull Vorbis-comment metadata out of the second header packet for the
    /// given stream and append it to the demuxer's metadata list.
    fn populate_metadata_for(&mut self, public_index: usize) {
        let codec_id = self.streams[public_index].params.codec_id.clone();
        let second_packet: Option<Vec<u8>> = self
            .state_by_serial
            .values()
            .find(|s| s.public_index == public_index)
            .and_then(|s| s.header_packets.get(1).cloned());
        let Some(p) = second_packet else { return };
        match codec_id.as_str() {
            "vorbis" if p.len() > 7 && &p[1..7] == b"vorbis" => {
                parse_vorbis_comment(&p[7..], &mut self.metadata);
            }
            "opus" if p.len() > 8 && &p[..8] == b"OpusTags" => {
                parse_vorbis_comment(&p[8..], &mut self.metadata);
            }
            "theora" if p.len() > 7 && &p[1..7] == b"theora" => {
                parse_vorbis_comment(&p[7..], &mut self.metadata);
            }
            _ => {}
        }
    }
}

impl Demuxer for OggDemuxer {
    fn format_name(&self) -> &str {
        "ogg"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.drain_next()? {
            return Ok(p);
        }
        Err(Error::Eof)
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        use std::io::SeekFrom;

        if stream_index as usize >= self.streams.len() {
            return Err(Error::invalid(format!(
                "Ogg: stream index {stream_index} out of range"
            )));
        }

        // Find the serial for the requested stream.
        let mut wanted_serial: Option<u32> = None;
        for (serial, state) in &self.state_by_serial {
            if self.streams[state.public_index].index == stream_index {
                wanted_serial = Some(*serial);
                break;
            }
        }
        let wanted_serial = wanted_serial.ok_or_else(|| {
            Error::unsupported(format!("Ogg: no logical stream for index {stream_index}"))
        })?;

        // For codecs the demuxer tracks (Vorbis, Opus, FLAC), the stream's
        // time_base already matches the native granule unit, so pts IS the
        // target granule. Theora and unknown streams use a microsecond base
        // and granule translation is codec-specific — reject that until we
        // grow a per-codec granule_to_pts helper.
        let codec_id = self.streams[stream_index as usize].params.codec_id.clone();
        let target_granule = match codec_id.as_str() {
            "vorbis" | "opus" | "flac" | "speex" => pts,
            _ => {
                return Err(Error::unsupported(format!(
                    "Ogg: seek_to not implemented for codec {}",
                    codec_id.as_str()
                )));
            }
        };

        // Determine the seekable byte range. `lo` is the current position
        // of the first page after the BOS/header section isn't known here —
        // we conservatively start at 0 and scan forward to the first OggS.
        let file_size = {
            let cur = self.input.stream_position()?;
            let end = self.input.seek(SeekFrom::End(0))?;
            self.input.seek(SeekFrom::Start(cur))?;
            end
        };
        if file_size == 0 {
            return Err(Error::unsupported("Ogg: empty input"));
        }

        // Fast path: if the seek index already has a `(granule, offset)`
        // entry with granule <= target_granule, jump to it directly.
        // The remaining linear-tail scan below still runs to tighten the
        // landing point against any indexed entries that were inserted
        // between the floor and the target — that scan reuses the index
        // too because `find_next_page_for_serial` records as it goes.
        if let Some((g, off)) = self.index_floor(wanted_serial, target_granule) {
            self.input.seek(SeekFrom::Start(off))?;
            self.page_queue.clear();
            self.out_queue.clear();
            for state in self.state_by_serial.values_mut() {
                state.pending.clear();
                state.granule_seen = 0;
            }
            self.eof_reached = false;
            // If `build_seek_index` ran, the index is dense and we know
            // there's no better page between `off` and the target —
            // return immediately. Otherwise (sparse index from incidental
            // scans), fall through to the bisection below to tighten.
            if self.seek_index_built {
                return Ok(g);
            }
            // Sparse case: seed the bisection so it can only improve the
            // landing point, never regress past `off`.
            // (Handled by falling through; the bisection's `landed`
            // tracker takes max() of each successful candidate.)
        }

        // Bisection state.
        let mut lo: u64 = 0;
        let mut hi: u64 = file_size;
        // Best-so-far: the last page with granule <= target_granule that
        // belongs to the requested stream. Tuple of (page_offset, granule).
        let mut landed: Option<(u64, i64)> = None;
        let threshold: u64 = 64 * 1024;

        // Upper bound on iterations: log2(file_size) + a handful for the
        // linear tail when the range shrinks below `threshold`.
        let max_iters: u32 = 64;
        let mut iters: u32 = 0;

        while lo < hi && iters < max_iters {
            iters += 1;
            let mid = lo + (hi - lo) / 2;
            // Scan forward from `mid` for the next OggS page belonging to
            // the target stream.
            let scan_limit: u64 = 64 * 1024;
            let scan_end = (mid + scan_limit).min(file_size);
            let (page_off, granule) =
                match self.find_next_page_for_serial(mid, scan_end, wanted_serial)? {
                    Some(v) => v,
                    None => {
                        // No matching page in this window — give up on the
                        // upper half and tighten `hi`.
                        hi = mid;
                        continue;
                    }
                };

            if granule <= target_granule {
                // This page is at or before target — remember it, try
                // later offsets.
                if landed.map(|(_, g)| granule >= g).unwrap_or(true) {
                    landed = Some((page_off, granule));
                }
                // Advance past this page's header to avoid re-landing on
                // the same page forever.
                lo = page_off + 1;
            } else {
                // granule > target — search the lower half.
                hi = page_off;
            }

            // Avoid pathological ping-pong on very small ranges.
            if hi.saturating_sub(lo) < threshold {
                break;
            }
        }

        // After bisection, do a bounded linear scan from `lo` toward `hi`
        // to tighten `landed` — this handles the final few pages inside
        // the threshold window without more bisection iterations.
        if let Some((_, _)) = landed {
            let mut cursor = landed.map(|(off, _)| off + 1).unwrap_or(lo);
            let scan_end = hi.min(cursor + threshold * 2);
            while cursor < scan_end {
                match self.find_next_page_for_serial(cursor, scan_end, wanted_serial)? {
                    Some((off, g)) => {
                        if g > target_granule {
                            break;
                        }
                        if landed.map(|(_, lg)| g >= lg).unwrap_or(true) {
                            landed = Some((off, g));
                        }
                        cursor = off + 1;
                    }
                    None => break,
                }
            }
        } else {
            // Bisection never found a page <= target. Try scanning from
            // the start one time — this covers files where every page's
            // granule exceeds the target (e.g., seek to pts 0 on a stream
            // whose first page already has granule > 0).
            if let Some((off, g)) = self.find_next_page_for_serial(0, file_size, wanted_serial)? {
                if g <= target_granule {
                    landed = Some((off, g));
                } else {
                    // Even the earliest page is past target — seek to it
                    // anyway; it's the best we can do.
                    landed = Some((off, g));
                }
            }
        }

        let (landed_off, landed_granule) = landed.ok_or_else(|| {
            Error::unsupported(format!(
                "Ogg: no seekable page found for stream {stream_index}"
            ))
        })?;

        // Seek the underlying input to the page boundary and flush all
        // buffered demuxer state so playback resumes cleanly.
        self.input.seek(SeekFrom::Start(landed_off))?;
        self.page_queue.clear();
        self.out_queue.clear();
        for state in self.state_by_serial.values_mut() {
            state.pending.clear();
            state.granule_seen = 0;
        }
        self.eof_reached = false;

        Ok(landed_granule)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }
}

/// Parse a Vorbis-comment payload. The input does NOT include any codec
/// magic prefix — the caller must strip it first. Appends (lowercase key,
/// value) pairs to `out`.
fn parse_vorbis_comment(buf: &[u8], out: &mut Vec<(String, String)>) {
    let mut i = 0usize;
    if buf.len() < 4 {
        return;
    }
    let vlen = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    i += 4;
    if i + vlen > buf.len() {
        return;
    }
    let vendor = String::from_utf8_lossy(&buf[i..i + vlen]).to_string();
    i += vlen;
    if !vendor.is_empty() {
        out.push(("vendor".into(), vendor));
    }
    if i + 4 > buf.len() {
        return;
    }
    let n = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
    i += 4;
    for _ in 0..n {
        if i + 4 > buf.len() {
            break;
        }
        let clen = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        if i + clen > buf.len() {
            break;
        }
        let entry = &buf[i..i + clen];
        i += clen;
        if let Some(eq) = entry.iter().position(|&b| b == b'=') {
            let key = String::from_utf8_lossy(&entry[..eq])
                .to_ascii_lowercase()
                .trim()
                .to_string();
            let value = String::from_utf8_lossy(&entry[eq + 1..]).trim().to_string();
            if !key.is_empty() && !value.is_empty() {
                out.push((key, value));
            }
        }
    }
}

/// Build initial codec parameters from a known identification packet.
fn guess_params(codec_id: &CodecId, first: &[u8]) -> Result<CodecParameters> {
    let mut p = match codec_id.as_str() {
        "vorbis" => CodecParameters::audio(codec_id.clone()),
        "opus" => CodecParameters::audio(codec_id.clone()),
        "flac" => CodecParameters::audio(codec_id.clone()),
        "theora" => CodecParameters::video(codec_id.clone()),
        "speex" => CodecParameters::audio(codec_id.clone()),
        _ => {
            let mut p = CodecParameters::audio(codec_id.clone());
            p.media_type = MediaType::Unknown;
            p
        }
    };

    match codec_id.as_str() {
        "vorbis" => parse_vorbis_id(&mut p, first)?,
        "opus" => parse_opus_id(&mut p, first)?,
        _ => {}
    }
    Ok(p)
}

fn parse_vorbis_id(p: &mut CodecParameters, packet: &[u8]) -> Result<()> {
    if packet.len() < 30 {
        return Err(Error::invalid("Vorbis identification header too short"));
    }
    // packet[0]=0x01, packet[1..7]="vorbis", packet[7..11]=version (must be 0).
    let version = u32::from_le_bytes([packet[7], packet[8], packet[9], packet[10]]);
    if version != 0 {
        return Err(Error::unsupported(format!(
            "unsupported Vorbis version {version}"
        )));
    }
    let channels = packet[11];
    let sample_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
    let _br_max = i32::from_le_bytes([packet[16], packet[17], packet[18], packet[19]]);
    let br_nom = i32::from_le_bytes([packet[20], packet[21], packet[22], packet[23]]);
    let _br_min = i32::from_le_bytes([packet[24], packet[25], packet[26], packet[27]]);
    if channels == 0 || sample_rate == 0 {
        return Err(Error::invalid("Vorbis ID header has zero channels or rate"));
    }
    p.channels = Some(channels as u16);
    p.sample_rate = Some(sample_rate);
    if br_nom > 0 {
        p.bit_rate = Some(br_nom as u64);
    }
    Ok(())
}

fn parse_opus_id(p: &mut CodecParameters, packet: &[u8]) -> Result<()> {
    if packet.len() < 19 {
        return Err(Error::invalid("Opus identification header too short"));
    }
    let channels = packet[9];
    let input_rate = u32::from_le_bytes([packet[12], packet[13], packet[14], packet[15]]);
    p.channels = Some(channels as u16);
    // Opus always decodes to 48 kHz; "input_sample_rate" is informational.
    p.sample_rate = Some(if input_rate > 0 { input_rate } else { 48_000 });
    Ok(())
}

/// Build the per-codec setup blob ("CodecPrivate" in Matroska, "esds"-equivalent
/// in MP4, etc.) from the header packets gathered out of an Ogg stream.
///
/// - Vorbis / Theora: Xiph-laced concatenation of all 3 header packets
///   (id, comment, setup) — one count byte (N-1) + Xiph-style sizes for the
///   first N-1 packets + packets concatenated. This is the layout the
///   corresponding decoders consume via `parse_xiph_extradata`.
/// - Opus: just the OpusHead identification packet (OpusTags discarded).
/// - Anything else: concatenate the headers and let the codec sort it out.
fn build_codec_private(codec_id: &CodecId, packets: &[Vec<u8>]) -> Vec<u8> {
    match codec_id.as_str() {
        "vorbis" | "theora" if packets.len() == 3 => xiph_lace_three(packets),
        "opus" => packets.first().cloned().unwrap_or_default(),
        _ => packets.iter().flatten().copied().collect(),
    }
}

/// Xiph-lace three header packets into the single-blob extradata format used
/// by Vorbis and Theora in MP4/MKV (and consumed by our per-codec decoders).
fn xiph_lace_three(packets: &[Vec<u8>]) -> Vec<u8> {
    debug_assert_eq!(packets.len(), 3);
    let mut out = Vec::with_capacity(
        1 + packets[0].len() / 255
            + 1
            + packets[1].len() / 255
            + 1
            + packets.iter().map(|p| p.len()).sum::<usize>(),
    );
    out.push(0x02); // 3 packets - 1
    out.extend(xiph_lace_size(packets[0].len()));
    out.extend(xiph_lace_size(packets[1].len()));
    out.extend_from_slice(&packets[0]);
    out.extend_from_slice(&packets[1]);
    out.extend_from_slice(&packets[2]);
    out
}

fn xiph_lace_size(mut n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n / 255 + 1);
    while n >= 255 {
        v.push(255);
        n -= 255;
    }
    v.push(n as u8);
    v
}

fn read_exact_or_eof(r: &mut dyn Read, buf: &mut [u8]) -> Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => {
                return if got == 0 {
                    Ok(false)
                } else {
                    Err(Error::invalid(format!(
                        "Ogg: truncated read ({}/{} bytes)",
                        got,
                        buf.len()
                    )))
                };
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}
