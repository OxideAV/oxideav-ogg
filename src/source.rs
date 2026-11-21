//! Position-tracking read window over the demuxer's input.
//!
//! The demuxer needs three things from its byte source that a bare
//! `Read + Seek` object does not give cheaply:
//!
//! * the **current offset** of every page (for the seek index and the
//!   damage ledger) without a position query per page;
//! * the ability to **look at a whole page before consuming it** — a
//!   page whose checksum fails must be rescanned from one byte past its
//!   capture pattern (RFC 3533 §6 field 1), which on a plain reader is
//!   a backward seek;
//! * **no seeking at all on the linear path**, so a live / pipe-like
//!   source whose `seek` fails still demuxes: capture loss, checksum
//!   failures and truncated tails are all resolved inside the window.
//!
//! [`PageSource`] keeps one fixed window of the input. `peek(n)`
//! guarantees up to `n` contiguous bytes at the cursor (fewer only at
//! end of input), `consume(n)` advances, and `Seek` requests that stay
//! inside the window are pointer moves. Only a seek that leaves the
//! window, or a seek relative to the end, reaches the underlying
//! source.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::ReadSeek;

/// Largest page RFC 3533 §6 permits: 27-byte header, 255 lacing values,
/// 255 × 255 body bytes.
pub(crate) const MAX_PAGE_BYTES: usize = 27 + 255 + 255 * 255;

/// Bytes requested from the underlying source per refill.
const REFILL_BYTES: usize = 64 * 1024;

/// Window capacity: always room for one maximum page plus one refill.
const WINDOW_BYTES: usize = MAX_PAGE_BYTES + REFILL_BYTES;

/// Largest `peek` request honoured in full.
pub(crate) const MAX_PEEK: usize = MAX_PAGE_BYTES;

pub(crate) struct PageSource {
    inner: Box<dyn ReadSeek>,
    /// The window. `window[cursor..filled]` is unread input.
    window: Box<[u8]>,
    cursor: usize,
    filled: usize,
    /// Absolute offset of `window[0]`. The underlying source's own
    /// position is always `origin + filled`.
    origin: u64,
    /// The underlying source reported end of input at `origin + filled`.
    /// Remembered so a scan near the end does not ask again for every
    /// byte it examines; forgotten on any reposition and whenever the
    /// demuxer starts a new page ([`PageSource::retry_end`]), so a
    /// source that grows is picked up again.
    ended: bool,
}

impl PageSource {
    pub(crate) fn new(mut inner: Box<dyn ReadSeek>) -> Self {
        // A source that cannot report its position is treated as
        // starting at offset 0; every later offset is counted from the
        // bytes actually read.
        let origin = inner.stream_position().unwrap_or(0);
        Self {
            inner,
            window: vec![0u8; WINDOW_BYTES].into_boxed_slice(),
            cursor: 0,
            filled: 0,
            origin,
            ended: false,
        }
    }

    /// Absolute offset of the next unread byte.
    pub(crate) fn position(&self) -> u64 {
        self.origin + self.cursor as u64
    }

    /// Make up to `n` (≤ [`MAX_PEEK`]) contiguous unread bytes available
    /// and return them without consuming. The slice is shorter than `n`
    /// only when the input ends first.
    pub(crate) fn peek(&mut self, n: usize) -> std::io::Result<&[u8]> {
        let n = n.min(MAX_PEEK);
        if self.filled - self.cursor < n {
            self.refill(n)?;
        }
        let end = self.filled.min(self.cursor + n);
        Ok(&self.window[self.cursor..end])
    }

    /// Advance the cursor by `n` bytes previously returned by `peek`.
    pub(crate) fn consume(&mut self, n: usize) {
        self.cursor = (self.cursor + n).min(self.filled);
    }

    fn refill(&mut self, want: usize) -> std::io::Result<()> {
        if self.cursor > 0 && self.window.len() - self.filled < REFILL_BYTES {
            // Slide the unread tail to the front so every read asks the
            // source for a full refill. `want <= MAX_PEEK` and the unread
            // tail is shorter than `want`, so after the slide a full
            // refill always fits.
            self.window.copy_within(self.cursor..self.filled, 0);
            self.origin += self.cursor as u64;
            self.filled -= self.cursor;
            self.cursor = 0;
        }
        while self.filled - self.cursor < want && !self.ended {
            let room = (self.window.len() - self.filled).min(REFILL_BYTES);
            match self
                .inner
                .read(&mut self.window[self.filled..self.filled + room])
            {
                Ok(0) => self.ended = true,
                Ok(got) => self.filled += got,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Ask the underlying source again on the next refill even if it
    /// reported end of input before.
    pub(crate) fn retry_end(&mut self) {
        self.ended = false;
    }

    /// The unread bytes already in the window (no I/O).
    pub(crate) fn buffered(&self) -> &[u8] {
        &self.window[self.cursor..self.filled]
    }

    fn jump(&mut self, target: u64) -> std::io::Result<u64> {
        if target >= self.origin && target <= self.origin + self.filled as u64 {
            self.cursor = (target - self.origin) as usize;
            return Ok(target);
        }
        // A short hop forward is cheaper read through than sought: the
        // bytes skipped are fewer than one refill, and the read that
        // follows a seek would have fetched a refill anyway.
        let buffered_end = self.origin + self.filled as u64;
        if target > buffered_end && target - buffered_end <= REFILL_BYTES as u64 {
            self.cursor = self.filled;
            let gap = (target - buffered_end) as usize;
            if self.peek(gap)?.len() == gap {
                self.consume(gap);
                return Ok(target);
            }
            // The input ended before the target; fall through so the
            // underlying source decides what a seek past its end means.
        }
        let at = self.inner.seek(SeekFrom::Start(target))?;
        self.origin = at;
        self.cursor = 0;
        self.filled = 0;
        self.ended = false;
        Ok(at)
    }

    /// Total length of the input, leaving the cursor and the window
    /// untouched. Fails on a source that cannot seek.
    pub(crate) fn total_len(&mut self) -> std::io::Result<u64> {
        let resume = self.origin + self.filled as u64;
        let end = self.inner.seek(SeekFrom::End(0))?;
        self.inner.seek(SeekFrom::Start(resume))?;
        Ok(end)
    }

    /// Read the last `max` bytes of the input (fewer when the input is
    /// shorter), leaving the cursor and the window untouched. Returns
    /// the absolute offset of the first returned byte with the bytes.
    pub(crate) fn read_tail(&mut self, max: u64) -> std::io::Result<(u64, Vec<u8>)> {
        let resume = self.origin + self.filled as u64;
        let end = self.inner.seek(SeekFrom::End(0))?;
        let start = end.saturating_sub(max);
        let outcome = self.read_span(start, (end - start) as usize);
        // Whatever happened, put the underlying position back where the
        // window expects it before reporting.
        self.inner.seek(SeekFrom::Start(resume))?;
        outcome.map(|bytes| (start, bytes))
    }

    fn read_span(&mut self, start: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.inner.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0u8; len];
        let mut got = 0;
        while got < len {
            match self.inner.read(&mut bytes[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        bytes.truncate(got);
        Ok(bytes)
    }
}

impl Read for PageSource {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let avail = self.peek(out.len())?;
        let n = avail.len();
        out[..n].copy_from_slice(avail);
        self.consume(n);
        Ok(n)
    }
}

impl Seek for PageSource {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match to {
            SeekFrom::Start(target) => self.jump(target),
            SeekFrom::Current(delta) => {
                let target = self
                    .position()
                    .checked_add_signed(delta)
                    .ok_or_else(|| std::io::Error::other("seek out of range"))?;
                self.jump(target)
            }
            SeekFrom::End(delta) => {
                let at = self.inner.seek(SeekFrom::End(delta))?;
                self.origin = at;
                self.cursor = 0;
                self.filled = 0;
                self.ended = false;
                Ok(at)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn source(len: usize) -> (Vec<u8>, PageSource) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let src = PageSource::new(Box::new(Cursor::new(data.clone())));
        (data, src)
    }

    #[test]
    fn peek_then_consume_walks_the_whole_input() {
        let (data, mut src) = source(400_000);
        let mut at = 0usize;
        let mut step = 1usize;
        while at < data.len() {
            let want = step.min(MAX_PEEK);
            let got = src.peek(want).unwrap().to_vec();
            let expect = &data[at..(at + want).min(data.len())];
            assert_eq!(got, expect);
            assert_eq!(src.position(), at as u64);
            src.consume(got.len());
            at += got.len();
            step = step * 3 + 7;
            if step > MAX_PEEK {
                step = 5;
            }
        }
        assert!(src.peek(10).unwrap().is_empty());
    }

    #[test]
    fn seek_inside_and_outside_the_window() {
        let (data, mut src) = source(300_000);
        assert_eq!(src.peek(100).unwrap(), &data[..100]);
        src.consume(100);
        src.seek(SeekFrom::Start(10)).unwrap();
        assert_eq!(src.peek(4).unwrap(), &data[10..14]);
        src.seek(SeekFrom::Start(250_000)).unwrap();
        assert_eq!(src.peek(4).unwrap(), &data[250_000..250_004]);
        assert_eq!(src.seek(SeekFrom::Current(-3)).unwrap(), 249_997);
        assert_eq!(src.peek(2).unwrap(), &data[249_997..249_999]);
        assert_eq!(src.seek(SeekFrom::End(0)).unwrap(), 300_000);
        assert!(src.peek(1).unwrap().is_empty());
    }

    #[test]
    fn tail_probe_leaves_the_cursor_alone() {
        let (data, mut src) = source(200_000);
        src.peek(50).unwrap();
        src.consume(20);
        assert_eq!(src.total_len().unwrap(), 200_000);
        let (start, tail) = src.read_tail(1000).unwrap();
        assert_eq!(start, 199_000);
        assert_eq!(tail, &data[199_000..]);
        assert_eq!(src.position(), 20);
        let mut rest = Vec::new();
        src.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, &data[20..]);
    }
}
