//! Bounded streaming RFC 1951 (raw DEFLATE) / RFC 1950 (zlib) / RFC 1952 (gzip)
//! decoder for `HttpClient`.
//!
//! Enforces independent caps on compressed wire bytes and decompressed output
//! bytes per chunk, supports multi-member gzip streams and auto-detects
//! zlib-wrapped vs raw RFC 1951 HTTP `deflate` (`httpfetch.py:164-169`).
//!
//! Pure safe Rust (no `unsafe`), written here because `fsm-net` cannot add
//! `async-compression` / `flate2` to `Cargo.toml` post-W0a.

use bytes::BytesMut;

/// Supported streaming encodings handled directly by this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InflateKind {
    /// `gzip` / `x-gzip` (RFC 1952, multi-member).
    Gzip,
    /// `deflate` (auto-detects RFC 1950 zlib wrapper vs raw RFC 1951).
    Deflate,
}

/// Outcome of feeding a wire chunk or finishing the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InflateStatus {
    /// Need more wire bytes (or `finish()`).
    Continue,
    /// Decompressed output exceeded `cap`; caller has the first `cap` bytes.
    Truncated,
    /// Compressed wire bytes exceeded `cap` before decompressed output hit `cap`.
    WireCapExceeded,
}

const WINDOW_SIZE: usize = 32_768;
const WINDOW_MASK: usize = WINDOW_SIZE - 1;

const CODE_LEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Canonical Huffman table (max 15 bits, RFC 1951).
#[derive(Debug, Clone)]
struct HuffTable {
    counts: [u16; 16],
    symbols: Vec<u16>,
    max_bits: usize,
}

impl HuffTable {
    fn from_lengths(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        let mut max_bits = 0usize;
        for &len in lengths {
            if len > 15 {
                return Err("Huffman code length > 15".into());
            }
            if len > 0 {
                let l = usize::from(len);
                counts[l] += 1;
                max_bits = max_bits.max(l);
            }
        }
        // Check for oversubscribed tree.
        let mut left = 1i32;
        for &c in counts.iter().skip(1) {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err("oversubscribed Huffman tree".into());
            }
        }
        let mut offsets = [0usize; 16];
        let mut total = 0usize;
        for l in 1..=15 {
            offsets[l] = total;
            total += usize::from(counts[l]);
        }
        let mut symbols = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                let l = usize::from(len);
                symbols[offsets[l]] = sym as u16;
                offsets[l] += 1;
            }
        }
        Ok(Self {
            counts,
            symbols,
            max_bits,
        })
    }

    fn fixed_litlen() -> Self {
        let mut lengths = [0u8; 288];
        lengths[0..=143].fill(8);
        lengths[144..=255].fill(9);
        lengths[256..=279].fill(7);
        lengths[280..=287].fill(8);
        Self::from_lengths(&lengths).expect("static fixed litlen table")
    }

    fn fixed_dist() -> Self {
        Self::from_lengths(&[5u8; 32]).expect("static fixed dist table")
    }

    /// Decode one symbol without consuming bits on `Ok(None)` (need more input).
    fn decode(&self, bits: &mut BitBuf) -> Result<Option<u16>, String> {
        if self.max_bits == 0 {
            return Err("empty Huffman table".into());
        }
        let mut code = 0u32;
        let mut first = 0u32;
        let mut index = 0usize;
        for len in 1..=self.max_bits {
            if usize::from(bits.nbits) < len {
                return Ok(None);
            }
            let bit = ((bits.bits >> (len - 1)) & 1) as u32;
            code = (code << 1) | bit;
            let count = u32::from(self.counts[len]);
            if code < first + count {
                bits.consume(len as u8);
                return Ok(Some(self.symbols[index + (code - first) as usize]));
            }
            index += count as usize;
            first = (first + count) << 1;
        }
        Err("invalid Huffman code".into())
    }
}

#[derive(Debug, Default)]
struct BitBuf {
    bits: u64,
    nbits: u8,
}

impl BitBuf {
    fn clear(&mut self) {
        self.bits = 0;
        self.nbits = 0;
    }

    fn fill(&mut self, src: &[u8], pos: &mut usize) {
        while self.nbits <= 56 && *pos < src.len() {
            self.bits |= u64::from(src[*pos]) << self.nbits;
            self.nbits += 8;
            *pos += 1;
        }
    }

    fn take(&mut self, n: u8) -> Option<u32> {
        if self.nbits < n {
            return None;
        }
        let mask = if n == 32 {
            u64::from(u32::MAX)
        } else {
            (1u64 << n) - 1
        };
        let v = (self.bits & mask) as u32;
        self.consume(n);
        Some(v)
    }

    fn consume(&mut self, n: u8) {
        self.bits >>= n;
        self.nbits -= n;
    }

    fn align_byte(&mut self) {
        let rem = self.nbits & 7;
        if rem != 0 {
            self.consume(rem);
        }
    }

    fn take_byte(&mut self, src: &[u8], pos: &mut usize) -> Option<u8> {
        if self.nbits >= 8 {
            let b = (self.bits & 0xff) as u8;
            self.consume(8);
            Some(b)
        } else if *pos < src.len() {
            let b = src[*pos];
            *pos += 1;
            Some(b)
        } else {
            None
        }
    }

    fn has_unread_bytes(&self) -> bool {
        self.nbits >= 8
    }
}

/// IEEE 802.3 CRC-32 (gzip).
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0)
    }

    fn update_byte(&mut self, b: u8) {
        let mut c = (!self.0) ^ u32::from(b);
        for _ in 0..8 {
            if c & 1 != 0 {
                c = (c >> 1) ^ 0xEDB8_8320;
            } else {
                c >>= 1;
            }
        }
        self.0 = !c;
    }

    fn value(&self) -> u32 {
        self.0
    }
}

/// Adler-32 (zlib).
struct Adler32 {
    s1: u32,
    s2: u32,
}

impl Adler32 {
    fn new() -> Self {
        Self { s1: 1, s2: 0 }
    }

    fn update_byte(&mut self, b: u8) {
        self.s1 = (self.s1 + u32::from(b)) % 65_521;
        self.s2 = (self.s2 + self.s1) % 65_521;
    }

    fn value(&self) -> u32 {
        (self.s2 << 16) | self.s1
    }
}

enum DeflateState {
    BlockHeader,
    StoredHeader,
    StoredData {
        rem: usize,
    },
    DynCounts,
    DynCodeLens {
        hlit: usize,
        hdist: usize,
        hclen: usize,
        idx: usize,
        cl_lens: [u8; 19],
    },
    DynLengths {
        hlit: usize,
        hdist: usize,
        cl_table: HuffTable,
        lens: Vec<u8>,
    },
    DynRepeat {
        hlit: usize,
        hdist: usize,
        cl_table: HuffTable,
        lens: Vec<u8>,
        sym: u16,
    },
    DecodeLitLen,
    ReadLenExtra {
        idx: usize,
    },
    DecodeDist {
        len: usize,
    },
    ReadDistExtra {
        len: usize,
        idx: usize,
    },
    CopyMatch {
        dist: usize,
        rem: usize,
    },
    Done,
}

struct InflateCore {
    bits: BitBuf,
    state: DeflateState,
    final_block: bool,
    litlen: HuffTable,
    dist: HuffTable,
    window: Vec<u8>,
    win_pos: usize,
    win_filled: usize,
}

impl InflateCore {
    fn new() -> Self {
        Self {
            bits: BitBuf::default(),
            state: DeflateState::BlockHeader,
            final_block: false,
            litlen: HuffTable::fixed_litlen(),
            dist: HuffTable::fixed_dist(),
            window: vec![0u8; WINDOW_SIZE],
            win_pos: 0,
            win_filled: 0,
        }
    }

    fn reset(&mut self) {
        self.bits.clear();
        self.state = DeflateState::BlockHeader;
        self.final_block = false;
        self.win_pos = 0;
        self.win_filled = 0;
    }

    fn is_done(&self) -> bool {
        matches!(self.state, DeflateState::Done)
    }

    fn push_byte(&mut self, b: u8, sink: &mut OutputSink<'_>) -> Result<bool, String> {
        self.window[self.win_pos] = b;
        self.win_pos = (self.win_pos + 1) & WINDOW_MASK;
        if self.win_filled < WINDOW_SIZE {
            self.win_filled += 1;
        }
        Ok(sink.push(b))
    }

    /// Returns `Ok(true)` if output hit truncation cap, `Ok(false)` otherwise.
    fn pump(
        &mut self,
        src: &[u8],
        pos: &mut usize,
        sink: &mut OutputSink<'_>,
    ) -> Result<bool, String> {
        loop {
            match &mut self.state {
                DeflateState::Done => return Ok(false),
                DeflateState::BlockHeader => {
                    self.bits.fill(src, pos);
                    let Some(hdr) = self.bits.take(3) else {
                        return Ok(false);
                    };
                    self.final_block = (hdr & 1) != 0;
                    match hdr >> 1 {
                        0 => {
                            self.bits.align_byte();
                            self.state = DeflateState::StoredHeader;
                        }
                        1 => {
                            self.litlen = HuffTable::fixed_litlen();
                            self.dist = HuffTable::fixed_dist();
                            self.state = DeflateState::DecodeLitLen;
                        }
                        2 => {
                            self.state = DeflateState::DynCounts;
                        }
                        _ => return Err("invalid DEFLATE block type 3".into()),
                    }
                }
                DeflateState::StoredHeader => {
                    self.bits.fill(src, pos);
                    let Some(word) = self.bits.take(32) else {
                        return Ok(false);
                    };
                    let len = (word & 0xffff) as u16;
                    let nlen = (word >> 16) as u16;
                    if len != !nlen {
                        return Err("corrupt DEFLATE stored block length".into());
                    }
                    self.state = DeflateState::StoredData {
                        rem: usize::from(len),
                    };
                }
                DeflateState::StoredData { rem } => {
                    let mut left = *rem;
                    while left > 0 {
                        let Some(b) = self.bits.take_byte(src, pos) else {
                            *rem = left;
                            return Ok(false);
                        };
                        left -= 1;
                        self.window[self.win_pos] = b;
                        self.win_pos = (self.win_pos + 1) & WINDOW_MASK;
                        if self.win_filled < WINDOW_SIZE {
                            self.win_filled += 1;
                        }
                        if sink.push(b) {
                            *rem = left;
                            return Ok(true);
                        }
                    }
                    self.state = if self.final_block {
                        self.bits.align_byte();
                        DeflateState::Done
                    } else {
                        DeflateState::BlockHeader
                    };
                }
                DeflateState::DynCounts => {
                    self.bits.fill(src, pos);
                    let Some(v) = self.bits.take(14) else {
                        return Ok(false);
                    };
                    let hlit = ((v & 0x1f) as usize) + 257;
                    let hdist = (((v >> 5) & 0x1f) as usize) + 1;
                    let hclen = (((v >> 10) & 0x0f) as usize) + 4;
                    if hlit > 286 || hdist > 32 {
                        return Err("invalid dynamic Huffman counts".into());
                    }
                    self.state = DeflateState::DynCodeLens {
                        hlit,
                        hdist,
                        hclen,
                        idx: 0,
                        cl_lens: [0u8; 19],
                    };
                }
                DeflateState::DynCodeLens {
                    hlit,
                    hdist,
                    hclen,
                    idx,
                    cl_lens,
                } => {
                    while *idx < *hclen {
                        self.bits.fill(src, pos);
                        let Some(v) = self.bits.take(3) else {
                            return Ok(false);
                        };
                        cl_lens[CODE_LEN_ORDER[*idx]] = v as u8;
                        *idx += 1;
                    }
                    let cl_table = HuffTable::from_lengths(cl_lens)?;
                    let (hlit, hdist) = (*hlit, *hdist);
                    self.state = DeflateState::DynLengths {
                        hlit,
                        hdist,
                        cl_table,
                        lens: Vec::with_capacity(hlit + hdist),
                    };
                }
                DeflateState::DynLengths {
                    hlit,
                    hdist,
                    cl_table,
                    lens,
                } => {
                    let target = *hlit + *hdist;
                    while lens.len() < target {
                        self.bits.fill(src, pos);
                        let Some(sym) = cl_table.decode(&mut self.bits)? else {
                            return Ok(false);
                        };
                        if sym <= 15 {
                            lens.push(sym as u8);
                        } else {
                            let DeflateState::DynLengths {
                                hlit,
                                hdist,
                                cl_table,
                                lens,
                            } = std::mem::replace(&mut self.state, DeflateState::Done)
                            else {
                                unreachable!();
                            };
                            self.state = DeflateState::DynRepeat {
                                hlit,
                                hdist,
                                cl_table,
                                lens,
                                sym,
                            };
                            break;
                        }
                    }
                    if let DeflateState::DynLengths {
                        hlit, hdist, lens, ..
                    } = &self.state
                        && lens.len() == *hlit + *hdist
                    {
                        if lens[256] == 0 {
                            return Err("dynamic Huffman table missing end-of-block symbol".into());
                        }
                        self.litlen = HuffTable::from_lengths(&lens[..*hlit])?;
                        self.dist = HuffTable::from_lengths(&lens[*hlit..])?;
                        self.state = DeflateState::DecodeLitLen;
                    }
                }
                DeflateState::DynRepeat {
                    hlit,
                    hdist,
                    cl_table,
                    lens,
                    sym,
                } => {
                    self.bits.fill(src, pos);
                    let (val, count) = match *sym {
                        16 => {
                            let Some(prev) = lens.last().copied() else {
                                return Err("Huffman repeat with no previous length".into());
                            };
                            let Some(extra) = self.bits.take(2) else {
                                return Ok(false);
                            };
                            (prev, 3 + extra as usize)
                        }
                        17 => {
                            let Some(extra) = self.bits.take(3) else {
                                return Ok(false);
                            };
                            (0u8, 3 + extra as usize)
                        }
                        18 => {
                            let Some(extra) = self.bits.take(7) else {
                                return Ok(false);
                            };
                            (0u8, 11 + extra as usize)
                        }
                        _ => return Err("invalid code-length symbol".into()),
                    };
                    if lens.len() + count > *hlit + *hdist {
                        return Err("Huffman code-length repeat overflow".into());
                    }
                    lens.resize(lens.len() + count, val);
                    let _ = cl_table;
                    let DeflateState::DynRepeat {
                        hlit,
                        hdist,
                        cl_table,
                        lens,
                        ..
                    } = std::mem::replace(&mut self.state, DeflateState::Done)
                    else {
                        unreachable!();
                    };
                    self.state = DeflateState::DynLengths {
                        hlit,
                        hdist,
                        cl_table,
                        lens,
                    };
                }
                DeflateState::DecodeLitLen => {
                    self.bits.fill(src, pos);
                    let Some(sym) = self.litlen.decode(&mut self.bits)? else {
                        return Ok(false);
                    };
                    match sym {
                        0..=255 => {
                            if self.push_byte(sym as u8, sink)? {
                                return Ok(true);
                            }
                        }
                        256 => {
                            self.state = if self.final_block {
                                self.bits.align_byte();
                                DeflateState::Done
                            } else {
                                DeflateState::BlockHeader
                            };
                        }
                        257..=285 => {
                            let idx = usize::from(sym - 257);
                            if LEN_EXTRA[idx] == 0 {
                                self.state = DeflateState::DecodeDist {
                                    len: usize::from(LEN_BASE[idx]),
                                };
                            } else {
                                self.state = DeflateState::ReadLenExtra { idx };
                            }
                        }
                        _ => return Err("invalid DEFLATE length symbol".into()),
                    }
                }
                DeflateState::ReadLenExtra { idx } => {
                    self.bits.fill(src, pos);
                    let extra_bits = LEN_EXTRA[*idx];
                    let Some(extra) = self.bits.take(extra_bits) else {
                        return Ok(false);
                    };
                    let len = usize::from(LEN_BASE[*idx]) + extra as usize;
                    self.state = DeflateState::DecodeDist { len };
                }
                DeflateState::DecodeDist { len } => {
                    self.bits.fill(src, pos);
                    let Some(sym) = self.dist.decode(&mut self.bits)? else {
                        return Ok(false);
                    };
                    if sym >= 30 {
                        return Err("invalid DEFLATE distance symbol".into());
                    }
                    let idx = usize::from(sym);
                    let len = *len;
                    if DIST_EXTRA[idx] == 0 {
                        let dist = usize::from(DIST_BASE[idx]);
                        if dist > self.win_filled {
                            return Err("DEFLATE distance exceeds history window".into());
                        }
                        self.state = DeflateState::CopyMatch { dist, rem: len };
                    } else {
                        self.state = DeflateState::ReadDistExtra { len, idx };
                    }
                }
                DeflateState::ReadDistExtra { len, idx } => {
                    self.bits.fill(src, pos);
                    let extra_bits = DIST_EXTRA[*idx];
                    let Some(extra) = self.bits.take(extra_bits) else {
                        return Ok(false);
                    };
                    let dist = usize::from(DIST_BASE[*idx]) + extra as usize;
                    if dist > self.win_filled {
                        return Err("DEFLATE distance exceeds history window".into());
                    }
                    self.state = DeflateState::CopyMatch { dist, rem: *len };
                }
                DeflateState::CopyMatch { dist, rem } => {
                    let dist = *dist;
                    let mut left = *rem;
                    while left > 0 {
                        let from = self.win_pos.wrapping_sub(dist) & WINDOW_MASK;
                        let b = self.window[from];
                        left -= 1;
                        self.window[self.win_pos] = b;
                        self.win_pos = (self.win_pos + 1) & WINDOW_MASK;
                        if self.win_filled < WINDOW_SIZE {
                            self.win_filled += 1;
                        }
                        if sink.push(b) {
                            *rem = left;
                            return Ok(true);
                        }
                    }
                    self.state = DeflateState::DecodeLitLen;
                }
            }
        }
    }
}

enum Checksum {
    None,
    Crc { crc: Crc32, isize: u32 },
    Adler(Adler32),
}

struct OutputSink<'a> {
    out: &'a mut BytesMut,
    cap: usize,
    checksum: &'a mut Checksum,
}

impl OutputSink<'_> {
    /// Returns `true` if pushing `b` would exceed `cap` (caller must truncate).
    fn push(&mut self, b: u8) -> bool {
        if self.out.len() >= self.cap {
            return true;
        }
        self.out.extend_from_slice(&[b]);
        match self.checksum {
            Checksum::None => {}
            Checksum::Crc { crc, isize } => {
                crc.update_byte(b);
                *isize = isize.wrapping_add(1);
            }
            Checksum::Adler(a) => a.update_byte(b),
        }
        false
    }
}

enum Wrapper {
    // Gzip states
    GzipHeader { buf: [u8; 10], got: usize },
    GzipExtraLen { flg: u8, buf: [u8; 2], got: usize },
    GzipExtra { flg: u8, rem: usize },
    GzipName { flg: u8 },
    GzipComment { flg: u8 },
    GzipHcrc { rem: usize },
    GzipBody,
    GzipTrailer { buf: [u8; 8], got: usize },
    GzipBetweenMembers,
    // Deflate states
    DeflateProbe { buf: [u8; 2], got: usize },
    ZlibBody,
    ZlibTrailer { buf: [u8; 4], got: usize },
    ZlibDone,
    RawDeflateBody,
    RawDeflateDone,
}

/// Bounded streaming decoder for `gzip`/`x-gzip` and `deflate`.
pub(crate) struct BoundedInflater {
    kind: InflateKind,
    wire_cap: u64,
    wire_seen: u64,
    out_cap: usize,
    out: BytesMut,
    wrapper: Wrapper,
    core: InflateCore,
    checksum: Checksum,
}

impl BoundedInflater {
    pub(crate) fn new(kind: InflateKind, cap: u64) -> Self {
        let (wrapper, checksum) = match kind {
            InflateKind::Gzip => (
                Wrapper::GzipHeader {
                    buf: [0; 10],
                    got: 0,
                },
                Checksum::Crc {
                    crc: Crc32::new(),
                    isize: 0,
                },
            ),
            InflateKind::Deflate => (
                Wrapper::DeflateProbe {
                    buf: [0; 2],
                    got: 0,
                },
                Checksum::None,
            ),
        };
        Self {
            kind,
            wire_cap: cap,
            wire_seen: 0,
            out_cap: usize::try_from(cap).unwrap_or(usize::MAX),
            out: BytesMut::new(),
            wrapper,
            core: InflateCore::new(),
            checksum,
        }
    }

    pub(crate) fn into_bytes(self) -> bytes::Bytes {
        self.out.freeze()
    }

    /// Feed a compressed wire chunk.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<InflateStatus, String> {
        if chunk.is_empty() {
            return Ok(InflateStatus::Continue);
        }
        // Feed at most up to `wire_cap` first so that a decompression bomb whose
        // first TCP chunk exceeds `wire_cap` still reports `Truncated` if output
        // hits `out_cap` within the allowed wire budget.
        let wire_room = self.wire_cap.saturating_sub(self.wire_seen);
        let take = usize::try_from(wire_room)
            .unwrap_or(usize::MAX)
            .min(chunk.len());
        if take > 0 {
            self.wire_seen = self.wire_seen.saturating_add(take as u64);
            if self.process_slice(&chunk[..take])? {
                return Ok(InflateStatus::Truncated);
            }
        }
        if take < chunk.len() {
            return Ok(InflateStatus::WireCapExceeded);
        }
        Ok(InflateStatus::Continue)
    }

    /// Signal EOF on the wire stream and verify trailers/completeness.
    pub(crate) fn finish(&mut self) -> Result<InflateStatus, String> {
        match &self.wrapper {
            Wrapper::GzipBetweenMembers | Wrapper::ZlibDone | Wrapper::RawDeflateDone => {
                Ok(InflateStatus::Continue)
            }
            Wrapper::DeflateProbe { buf, got } if *got == 1 => {
                // Single-byte stream: try as raw deflate (will error as truncated).
                let byte = buf[0];
                self.wrapper = Wrapper::RawDeflateBody;
                if self.process_slice(&[byte])? {
                    return Ok(InflateStatus::Truncated);
                }
                if matches!(self.wrapper, Wrapper::RawDeflateDone) {
                    Ok(InflateStatus::Continue)
                } else {
                    Err("truncated deflate stream".into())
                }
            }
            _ => Err(match self.kind {
                InflateKind::Gzip => "truncated gzip stream".into(),
                InflateKind::Deflate => "truncated deflate stream".into(),
            }),
        }
    }

    /// Advance wrapper + deflate state machine over `src`. Returns `Ok(true)` if
    /// decompressed output hit `out_cap` (truncated).
    fn process_slice(&mut self, src: &[u8]) -> Result<bool, String> {
        let mut pos = 0usize;
        loop {
            match &mut self.wrapper {
                Wrapper::GzipBetweenMembers => {
                    if pos >= src.len() {
                        return Ok(false);
                    }
                    // Start a concatenated gzip member (RFC 1952 §2.2).
                    self.core.reset();
                    self.checksum = Checksum::Crc {
                        crc: Crc32::new(),
                        isize: 0,
                    };
                    self.wrapper = Wrapper::GzipHeader {
                        buf: [0; 10],
                        got: 0,
                    };
                }
                Wrapper::GzipHeader { buf, got } => {
                    while *got < 10 && pos < src.len() {
                        buf[*got] = src[pos];
                        *got += 1;
                        pos += 1;
                    }
                    if *got < 10 {
                        return Ok(false);
                    }
                    if buf[0] != 0x1f || buf[1] != 0x8b {
                        return Err("invalid gzip magic".into());
                    }
                    if buf[2] != 8 {
                        return Err(format!("unsupported gzip compression method {}", buf[2]));
                    }
                    let flg = buf[3];
                    if flg & 0xE0 != 0 {
                        return Err("invalid gzip reserved header flags".into());
                    }
                    self.wrapper = next_gzip_header_state(flg, 0);
                }
                Wrapper::GzipExtraLen { flg, buf, got } => {
                    while *got < 2 && pos < src.len() {
                        buf[*got] = src[pos];
                        *got += 1;
                        pos += 1;
                    }
                    if *got < 2 {
                        return Ok(false);
                    }
                    let xlen = usize::from(u16::from_le_bytes(*buf));
                    let flg = *flg;
                    self.wrapper = if xlen > 0 {
                        Wrapper::GzipExtra { flg, rem: xlen }
                    } else {
                        next_gzip_header_state(flg, 1)
                    };
                }
                Wrapper::GzipExtra { flg, rem } => {
                    let skip = (*rem).min(src.len() - pos);
                    pos += skip;
                    *rem -= skip;
                    if *rem > 0 {
                        return Ok(false);
                    }
                    let flg = *flg;
                    self.wrapper = next_gzip_header_state(flg, 1);
                }
                Wrapper::GzipName { flg } => {
                    let flg = *flg;
                    if let Some(rel) = src[pos..].iter().position(|&b| b == 0) {
                        pos += rel + 1;
                        self.wrapper = next_gzip_header_state(flg, 2);
                    } else {
                        return Ok(false);
                    }
                }
                Wrapper::GzipComment { flg } => {
                    let flg = *flg;
                    if let Some(rel) = src[pos..].iter().position(|&b| b == 0) {
                        pos += rel + 1;
                        self.wrapper = next_gzip_header_state(flg, 3);
                    } else {
                        return Ok(false);
                    }
                }
                Wrapper::GzipHcrc { rem } => {
                    let skip = (*rem).min(src.len() - pos);
                    pos += skip;
                    *rem -= skip;
                    if *rem > 0 {
                        return Ok(false);
                    }
                    self.wrapper = Wrapper::GzipBody;
                }
                Wrapper::GzipBody => {
                    let mut sink = OutputSink {
                        out: &mut self.out,
                        cap: self.out_cap,
                        checksum: &mut self.checksum,
                    };
                    if self.core.pump(src, &mut pos, &mut sink)? {
                        return Ok(true);
                    }
                    if self.core.is_done() {
                        self.wrapper = Wrapper::GzipTrailer {
                            buf: [0; 8],
                            got: 0,
                        };
                    } else {
                        return Ok(false);
                    }
                }
                Wrapper::GzipTrailer { buf, got } => {
                    while *got < 8 {
                        let Some(b) = self.core.bits.take_byte(src, &mut pos) else {
                            return Ok(false);
                        };
                        buf[*got] = b;
                        *got += 1;
                    }
                    let want_crc = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                    let want_isize = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                    if let Checksum::Crc { crc, isize } = &self.checksum {
                        if crc.value() != want_crc {
                            return Err("gzip CRC32 mismatch".into());
                        }
                        if *isize != want_isize {
                            return Err("gzip ISIZE mismatch".into());
                        }
                    }
                    // Carry over any buffered bytes in `core.bits` to the next member.
                    let mut leftover = Vec::new();
                    while self.core.bits.has_unread_bytes() {
                        if let Some(b) = self.core.bits.take_byte(&[], &mut 0) {
                            leftover.push(b);
                        }
                    }
                    self.wrapper = Wrapper::GzipBetweenMembers;
                    if !leftover.is_empty() && self.process_slice(&leftover)? {
                        return Ok(true);
                    }
                }
                Wrapper::DeflateProbe { buf, got } => {
                    while *got < 2 && pos < src.len() {
                        buf[*got] = src[pos];
                        *got += 1;
                        pos += 1;
                    }
                    if *got < 2 {
                        return Ok(false);
                    }
                    let [b0, b1] = *buf;
                    let wrapped = (b0 & 0x0f) == 8
                        && (b0 >> 4) <= 7
                        && u16::from_be_bytes([b0, b1]).is_multiple_of(31);
                    if wrapped {
                        if b1 & 0x20 != 0 {
                            return Err("zlib preset dictionary is not supported".into());
                        }
                        self.checksum = Checksum::Adler(Adler32::new());
                        self.wrapper = Wrapper::ZlibBody;
                    } else {
                        self.checksum = Checksum::None;
                        self.wrapper = Wrapper::RawDeflateBody;
                        if self.process_slice(&[b0, b1])? {
                            return Ok(true);
                        }
                    }
                }
                Wrapper::ZlibBody => {
                    let mut sink = OutputSink {
                        out: &mut self.out,
                        cap: self.out_cap,
                        checksum: &mut self.checksum,
                    };
                    if self.core.pump(src, &mut pos, &mut sink)? {
                        return Ok(true);
                    }
                    if self.core.is_done() {
                        self.wrapper = Wrapper::ZlibTrailer {
                            buf: [0; 4],
                            got: 0,
                        };
                    } else {
                        return Ok(false);
                    }
                }
                Wrapper::ZlibTrailer { buf, got } => {
                    while *got < 4 {
                        let Some(b) = self.core.bits.take_byte(src, &mut pos) else {
                            return Ok(false);
                        };
                        buf[*got] = b;
                        *got += 1;
                    }
                    let want = u32::from_be_bytes(*buf);
                    if let Checksum::Adler(a) = &self.checksum
                        && a.value() != want
                    {
                        return Err("zlib Adler-32 mismatch".into());
                    }
                    self.wrapper = Wrapper::ZlibDone;
                    if self.core.bits.has_unread_bytes() || pos < src.len() {
                        return Err("unexpected trailing data after deflate response".into());
                    }
                }
                Wrapper::ZlibDone => {
                    if pos < src.len() {
                        return Err("unexpected trailing data after deflate response".into());
                    }
                    return Ok(false);
                }
                Wrapper::RawDeflateBody => {
                    let mut sink = OutputSink {
                        out: &mut self.out,
                        cap: self.out_cap,
                        checksum: &mut self.checksum,
                    };
                    if self.core.pump(src, &mut pos, &mut sink)? {
                        return Ok(true);
                    }
                    if self.core.is_done() {
                        self.wrapper = Wrapper::RawDeflateDone;
                        if self.core.bits.has_unread_bytes() || pos < src.len() {
                            return Err("unexpected trailing data after deflate response".into());
                        }
                    } else {
                        return Ok(false);
                    }
                }
                Wrapper::RawDeflateDone => {
                    if pos < src.len() {
                        return Err("unexpected trailing data after deflate response".into());
                    }
                    return Ok(false);
                }
            }
        }
    }
}

/// Next gzip optional-header state after `stage` (0=fixed, 1=extra, 2=name, 3=comment).
fn next_gzip_header_state(flg: u8, stage: u8) -> Wrapper {
    if stage < 1 && (flg & 0x04) != 0 {
        return Wrapper::GzipExtraLen {
            flg,
            buf: [0; 2],
            got: 0,
        };
    }
    if stage < 2 && (flg & 0x08) != 0 {
        return Wrapper::GzipName { flg };
    }
    if stage < 3 && (flg & 0x10) != 0 {
        return Wrapper::GzipComment { flg };
    }
    if stage < 4 && (flg & 0x02) != 0 {
        return Wrapper::GzipHcrc { rem: 2 };
    }
    Wrapper::GzipBody
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_chunked(
        kind: InflateKind,
        data: &[u8],
        chunk_size: usize,
        cap: u64,
    ) -> Result<(bytes::Bytes, InflateStatus), String> {
        let mut inf = BoundedInflater::new(kind, cap);
        for chunk in data.chunks(chunk_size) {
            match inf.feed(chunk)? {
                InflateStatus::Continue => {}
                other => return Ok((inf.into_bytes(), other)),
            }
        }
        let st = inf.finish()?;
        Ok((inf.into_bytes(), st))
    }

    #[test]
    fn decodes_gzip_and_zlib_fixtures_across_chunk_sizes() {
        let gz = include_bytes!("../tests/fixtures/hello.gz");
        let zl = include_bytes!("../tests/fixtures/hello.zlib");
        for cs in [1, 2, 3, 7, 64, 4096] {
            let (out, st) = decode_chunked(InflateKind::Gzip, gz, cs, 1024).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(&out[..], b"hello, compressed world\n");

            let (out, st) = decode_chunked(InflateKind::Deflate, zl, cs, 1024).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(&out[..], b"hello, compressed world\n");
        }
    }

    #[test]
    fn decodes_multi_member_gzip_and_raw_deflate() {
        let gz = include_bytes!("../tests/fixtures/hello.gz");
        let mut two = Vec::with_capacity(gz.len() * 2);
        two.extend_from_slice(gz);
        two.extend_from_slice(gz);
        for cs in [1, 5, 64] {
            let (out, st) = decode_chunked(InflateKind::Gzip, &two, cs, 4096).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(
                &out[..],
                b"hello, compressed world\nhello, compressed world\n"
            );
        }

        // Raw RFC 1951 DEFLATE (strip 2-byte zlib header and 4-byte Adler-32 trailer).
        let zl = include_bytes!("../tests/fixtures/hello.zlib");
        let raw = &zl[2..zl.len() - 4];
        for cs in [1, 2, 13, 64] {
            let (out, st) = decode_chunked(InflateKind::Deflate, raw, cs, 1024).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(&out[..], b"hello, compressed world\n");
        }
    }

    #[test]
    fn bomb_truncates_at_output_cap_and_full_decode_matches() {
        let bomb = include_bytes!("../tests/fixtures/bomb_10mib_zeros.gz");
        let (out, st) = decode_chunked(InflateKind::Gzip, bomb, 512, 256 * 1024).unwrap();
        assert_eq!(st, InflateStatus::Truncated);
        assert_eq!(out.len(), 256 * 1024);
        assert!(out.iter().all(|&b| b == 0));

        let (out, st) = decode_chunked(InflateKind::Gzip, bomb, 4096, 20 << 20).unwrap();
        assert_eq!(st, InflateStatus::Continue);
        assert_eq!(out.len(), 10 << 20);
        assert!(out.iter().all(|&b| b == 0));
    }

    #[test]
    fn fcomment_wire_bomb_hits_wire_cap_with_zero_output() {
        // Valid gzip header with FCOMMENT (0x10) followed by 100 KiB of comment bytes.
        let mut wire = vec![0x1f, 0x8b, 0x08, 0x10, 0, 0, 0, 0, 0, 0xff];
        wire.extend(std::iter::repeat_n(b'A', 100_000));
        let (out, st) = decode_chunked(InflateKind::Gzip, &wire, 1024, 4096).unwrap();
        assert_eq!(st, InflateStatus::WireCapExceeded);
        assert!(out.is_empty());
    }

    #[test]
    fn decodes_stored_blocks_and_gzip_optional_headers_and_rejects_corrupt_streams() {
        // Raw DEFLATE stored block (BFINAL=1, BTYPE=00) containing "stored!".
        let payload = b"stored!";
        let len = payload.len() as u16;
        let nlen = !len;
        let mut raw_stored = vec![0x01];
        raw_stored.extend_from_slice(&len.to_le_bytes());
        raw_stored.extend_from_slice(&nlen.to_le_bytes());
        raw_stored.extend_from_slice(payload);
        for cs in [1, 2, 16] {
            let (out, st) = decode_chunked(InflateKind::Deflate, &raw_stored, cs, 1024).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(&out[..], payload);
        }

        // Gzip with FEXTRA | FNAME | FCOMMENT | FHCRC all set around the stored block.
        let mut crc = Crc32::new();
        for &b in payload {
            crc.update_byte(b);
        }
        let mut gz = vec![
            0x1f, 0x8b, 0x08, 0x1e, // FEXTRA(4) | FNAME(8) | FCOMMENT(16) | FHCRC(2)
            0, 0, 0, 0, 0, 0xff,
        ];
        gz.extend_from_slice(&3u16.to_le_bytes());
        gz.extend_from_slice(b"ext");
        gz.extend_from_slice(b"file.txt\0");
        gz.extend_from_slice(b"note\0");
        gz.extend_from_slice(&[0xaa, 0xbb]); // FHCRC
        gz.extend_from_slice(&raw_stored);
        gz.extend_from_slice(&crc.value().to_le_bytes());
        gz.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        for cs in [1, 3, 64] {
            let (out, st) = decode_chunked(InflateKind::Gzip, &gz, cs, 1024).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(&out[..], payload);
        }

        // Corrupt gzip CRC32 rejected.
        let mut bad_crc = gz.clone();
        let last = bad_crc.len() - 5;
        bad_crc[last] ^= 0xff;
        assert!(decode_chunked(InflateKind::Gzip, &bad_crc, 64, 1024).is_err());

        // Truncated gzip rejected.
        assert!(decode_chunked(InflateKind::Gzip, &gz[..gz.len() - 3], 64, 1024).is_err());

        // Trailing garbage after deflate rejected.
        let mut trailing = raw_stored.clone();
        trailing.push(0x99);
        assert!(decode_chunked(InflateKind::Deflate, &trailing, 64, 1024).is_err());

        // Corrupt NLEN in stored block rejected.
        let mut bad_nlen = raw_stored;
        bad_nlen[3] ^= 0xff;
        assert!(decode_chunked(InflateKind::Deflate, &bad_nlen, 64, 1024).is_err());
    }

    #[test]
    fn decodes_dynamic_huffman_gzip_zlib_and_raw_deflate() {
        use base64::Engine;
        let expected: String = (0..40)
            .map(|i| {
                format!(
                    "item-{i:03}: alpha={}, beta=0x{:04x}, tag=search_mcp_{}, cjk=异常检测_{}\n",
                    (i * 17) % 97,
                    i * 31,
                    i % 13,
                    i % 5
                )
            })
            .collect();
        let gz = base64::engine::general_purpose::STANDARD
            .decode(
                "H4sIAAAAAAAA/42UQWrcUBBE9z6FD5CAuvtLXz8wZxkkjSZ2EoNJZpFl8NbHyBkCXuQ69jkyIXRNkW7ErIS0eNClenV/Wh/ed1334Xb68ng37bp3t/N6Oj+/nz+eX07Tx923dfq63O0flsf9+cvy6fPu9ffT68vL288fb7+e993N/T+IOEQqUeQYKJJQxCnqFCtEsTVQNKGoU8wpvRClPwSKJRRzSnHKMBKlLoFSEkpxSu+UsSdKmwOl30p3wEUEmacAGbbCrQ5RJcqhBUrdCnfEL2pEOY6BMm6F23DQcKHI3+78R2kb4QqaW40oNsTSbXVX0N1GBkjfR8xWeQXlFcbUEjFb7RW0V8kkaXaVjx6woL2FTJJZr/IRAaO9A5kkB7nKR+SL9lYySY5xYTIfES/q20gC7eLCZD4iXdRXmKJxYTIfkS7qa6SSlrgwmZCerqK+hVTSIS5MJqSnq2jvQCrpGBcmE9LTVZR3JJV0ihOTCenpKrrLkCUuTOqjp6vorpJIusaJSX1EvCivkUjWxY1JfUS+l+0lk0zjyGQ3IV+0t5JJVpKN2coX7R3JJBvixGQXIV+0lyFjHJhMR0/XUF4lkWyKA5Pp6OEayltIJFviwGQ6eriG8vYkkq1xYDIdPVxDeSuJZMc4MJmOHq6hvI0cKBIHJtMR6aK7whSLA5PpiHRRXSORSh8HJvUR8aK7hUQqNS5M6iPyRXkHEqm0ODGpjwj40l4yqcxxZLKbys0fFTFVYfUKAAA=",
            )
            .unwrap();
        let zl = base64::engine::general_purpose::STANDARD
            .decode(
                "eJyNlEFq3FAQRPc+hQ+QgLr7S18/MGcZJI0mdhKDSWaRZfDWx8gZAl7kOvY5MiF0TZFuxKyEtHjQpXp1f1of3ndd9+F2+vJ4N+26d7fzejo/v58/nl9O08fdt3X6utztH5bH/fnL8unz7vX30+vLy9vPH2+/nvfdzf0/iDhEKlHkGCiSUMQp6hQrRLE1UDShqFPMKb0QpT8EiiUUc0pxyjASpS6BUhJKcUrvlLEnSpsDpd9Kd8BFBJmnABm2wq0OUSXKoQVK3Qp3xC9qRDmOgTJuhdtw0HChyN/u/EdpG+EKmluNKDbE0m11V9DdRgZI30fMVnkF5RXG1BIxW+0VtFfJJGl2lY8esKC9hUySWa/yEQGjvQOZJAe5ykfki/ZWMkmOcWEyHxEv6ttIAu3iwmQ+Il3UV5iicWEyH5Eu6mukkpa4MJmQnq6ivoVU0iEuTCakp6to70Aq6RgXJhPS01WUdySVdIoTkwnp6Sq6y5AlLkzqo6er6K6SSLrGiUl9RLwor5FI1sWNSX1EvpftJZNM48hkNyFftLeSSVaSjdnKF+0dySQb4sRkFyFftJchYxyYTEdP11BeJZFsigOT6ejhGspbSCRb4sBkOnq4hvL2JJKtcWAyHT1cQ3kriWTHODCZjh6uobyNHCgSBybTEemiu8IUiwOT6Yh0UV0jkUofByb1EfGiu4VEKjUuTOoj8kV5BxKptDgxqY8I+NJeMqnMcWSym8rNH85uKvE=",
            )
            .unwrap();
        assert_eq!(
            (zl[2] >> 1) & 3,
            2,
            "must exercise BTYPE=10 dynamic Huffman"
        );
        let raw = &zl[2..zl.len() - 4];

        for cs in [1, 2, 7, 64] {
            let (out, st) = decode_chunked(InflateKind::Gzip, &gz, cs, 8192).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(out.as_ref(), expected.as_bytes());

            let (out, st) = decode_chunked(InflateKind::Deflate, &zl, cs, 8192).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(out.as_ref(), expected.as_bytes());

            let (out, st) = decode_chunked(InflateKind::Deflate, raw, cs, 8192).unwrap();
            assert_eq!(st, InflateStatus::Continue);
            assert_eq!(out.as_ref(), expected.as_bytes());
        }
        // Corrupt Adler-32 trailer rejected.
        let mut bad_zl = zl;
        let last = bad_zl.len() - 1;
        bad_zl[last] ^= 0xff;
        assert!(decode_chunked(InflateKind::Deflate, &bad_zl, 64, 8192).is_err());
    }
}
