//! RIFF/WAVE reading and writing, std only.
//!
//! Reading: PCM 8/16/24/32-bit integer, IEEE float 32/64, WAVE_FORMAT_EXTENSIBLE and RF64, any
//! channel count. Samples are streamed in blocks (`read_frames`), so long files are never held in
//! memory. Unsupported encodings, truncated files and broken chunks give clear errors.
//!
//! Writing: 24-bit PCM or 32-bit float. Writers take a brand-new file (see `util::OutFile`).
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    Int,
    Float,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
    /// Container bits per sample.
    pub bits: u16,
    pub valid_bits: u16,
    pub block_align: u16,
    pub channel_mask: Option<u32>,
    pub frames: u64,
    pub data_offset: u64,
    pub rf64: bool,
    /// Things that were wrong but readable (missing pad byte, partial last frame, ...).
    pub warnings: Vec<String>,
}

impl WavInfo {
    pub fn duration_s(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }

    pub fn describe(&self) -> String {
        let kind = match self.format {
            SampleFormat::Int => "PCM",
            SampleFormat::Float => "float",
        };
        format!(
            "{kind} {}-bit, {} ch, {} Hz{}",
            self.valid_bits,
            self.channels,
            self.sample_rate,
            if self.rf64 { ", RF64" } else { "" }
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WavError {
    Io(String),
    NotWav(String),
    Unsupported(String),
    Truncated(String),
    BadChunk(String),
    TooLarge(String),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::Io(s) => write!(f, "I/O error: {s}"),
            WavError::NotWav(s) => write!(f, "not a WAV file: {s}"),
            WavError::Unsupported(s) => write!(f, "unsupported WAV: {s}"),
            WavError::Truncated(s) => write!(f, "truncated WAV: {s}"),
            WavError::BadChunk(s) => write!(f, "broken WAV chunk: {s}"),
            WavError::TooLarge(s) => write!(f, "WAV too large: {s}"),
        }
    }
}

impl From<io::Error> for WavError {
    fn from(e: io::Error) -> Self {
        WavError::Io(e.to_string())
    }
}

pub const MAX_CHANNELS: u16 = 256;
pub const MIN_RATE: u32 = 1_000;
pub const MAX_RATE: u32 = 1_536_000;

/// Tail shared by the KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT GUIDs (after the 2-byte format tag).
const GUID_TAIL: [u8; 14] = [
    0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

fn tag_name(tag: u16) -> &'static str {
    match tag {
        0x0002 => "Microsoft ADPCM",
        0x0006 => "A-law",
        0x0007 => "mu-law",
        0x0011 => "IMA ADPCM",
        0x0031 => "GSM 6.10",
        0x0050 => "MPEG",
        0x0055 => "MP3",
        0x0161 => "WMA",
        0x2000 => "AC-3",
        0x2001 => "DTS",
        _ => "unknown encoding",
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

fn fourcc_ok(id: &[u8]) -> bool {
    id.len() == 4 && id.iter().all(|c| (0x20..=0x7e).contains(c))
}

fn fourcc(id: &[u8]) -> String {
    if fourcc_ok(id) {
        String::from_utf8_lossy(id).into_owned()
    } else {
        format!("{id:02x?}")
    }
}

struct Fmt {
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    block_align: u16,
    bits: u16,
    valid_bits: u16,
    mask: Option<u32>,
}

fn parse_fmt(b: &[u8]) -> Result<Fmt, WavError> {
    let tag = u16_at(b, 0);
    let channels = u16_at(b, 2);
    let sample_rate = u32_at(b, 4);
    let block_align = u16_at(b, 12);
    let bits = u16_at(b, 14);
    let (format, mut valid_bits, mask) = match tag {
        1 => (SampleFormat::Int, bits, None),
        3 => (SampleFormat::Float, bits, None),
        0xFFFE => {
            if b.len() < 40 {
                return Err(WavError::BadChunk(format!(
                    "WAVE_FORMAT_EXTENSIBLE fmt chunk is {} bytes; it needs 40",
                    b.len()
                )));
            }
            let sub = u16_at(b, 24);
            if b[26..40] != GUID_TAIL {
                return Err(WavError::Unsupported(
                    "WAVE_FORMAT_EXTENSIBLE with a non-standard sub-format GUID".into(),
                ));
            }
            let f = match sub {
                1 => SampleFormat::Int,
                3 => SampleFormat::Float,
                other => {
                    return Err(WavError::Unsupported(format!(
                        "extensible sub-format 0x{other:04x} ({}); convert it to PCM or float WAV first",
                        tag_name(other)
                    )))
                }
            };
            (f, u16_at(b, 18), Some(u32_at(b, 20)))
        }
        other => {
            return Err(WavError::Unsupported(format!(
                "format tag 0x{other:04x} ({}); convert it to PCM or float WAV first (REAPER can render one)",
                tag_name(other)
            )))
        }
    };
    if channels == 0 {
        return Err(WavError::BadChunk("fmt chunk says 0 channels".into()));
    }
    if sample_rate == 0 {
        return Err(WavError::BadChunk("fmt chunk says sample rate 0".into()));
    }
    // Buffers and filters are sized from these two numbers, so a hostile header must not pick them.
    if channels > MAX_CHANNELS {
        return Err(WavError::Unsupported(format!(
            "{channels} channels (at most {MAX_CHANNELS})"
        )));
    }
    if !(MIN_RATE..=MAX_RATE).contains(&sample_rate) {
        return Err(WavError::Unsupported(format!(
            "sample rate {sample_rate} Hz (supported: {MIN_RATE} to {MAX_RATE})"
        )));
    }
    let ok_bits = match format {
        SampleFormat::Int => matches!(bits, 8 | 16 | 24 | 32),
        SampleFormat::Float => matches!(bits, 32 | 64),
    };
    if !ok_bits {
        let kind = if format == SampleFormat::Int {
            "integer PCM"
        } else {
            "float"
        };
        return Err(WavError::Unsupported(format!("{bits}-bit {kind}")));
    }
    let expect = channels as u32 * (bits as u32 / 8);
    if block_align as u32 != expect {
        return Err(WavError::BadChunk(format!(
            "block align {block_align} does not match {channels} channels of {bits} bits ({expect})"
        )));
    }
    if valid_bits == 0 {
        valid_bits = bits;
    }
    if valid_bits > bits {
        return Err(WavError::BadChunk(format!(
            "{valid_bits} valid bits in a {bits}-bit container"
        )));
    }
    Ok(Fmt {
        format,
        channels,
        sample_rate,
        block_align,
        bits,
        valid_bits,
        mask,
    })
}

/// Read 8 bytes at `pos` and say whether they look like a chunk header that fits in the file.
fn plausible_chunk<R: Read + Seek>(r: &mut R, pos: u64, file_len: u64) -> bool {
    if pos + 8 > file_len || r.seek(SeekFrom::Start(pos)).is_err() {
        return false;
    }
    let mut h = [0u8; 8];
    if r.read_exact(&mut h).is_err() || !fourcc_ok(&h[0..4]) {
        return false;
    }
    let size = u32_at(&h, 4) as u64;
    size <= file_len - pos - 8 || &h[0..4] == b"data"
}

fn parse_header<R: Read + Seek>(r: &mut R, file_len: u64) -> Result<WavInfo, WavError> {
    if file_len < 12 {
        return Err(WavError::Truncated(format!(
            "the file is {file_len} bytes; a WAV header needs at least 12"
        )));
    }
    let mut head = [0u8; 12];
    r.read_exact(&mut head)?;
    let rf64 = match &head[0..4] {
        b"RIFF" => false,
        b"RF64" | b"BW64" => true,
        b"RIFX" => return Err(WavError::Unsupported("big-endian RIFX WAV".into())),
        b"riff" => return Err(WavError::Unsupported("Sony Wave64 (.w64)".into())),
        b"FORM" => return Err(WavError::NotWav("this is an AIFF file".into())),
        b"fLaC" => return Err(WavError::NotWav("this is a FLAC file".into())),
        b"OggS" => return Err(WavError::NotWav("this is an Ogg file".into())),
        x if &x[0..3] == b"ID3" || (x[0] == 0xFF && x[1] & 0xE0 == 0xE0) => {
            return Err(WavError::NotWav("this looks like an MP3 file".into()))
        }
        other => {
            return Err(WavError::NotWav(format!(
                "no RIFF header (starts with {})",
                fourcc(other)
            )))
        }
    };
    if &head[8..12] != b"WAVE" {
        return Err(WavError::NotWav(format!(
            "RIFF form type is {}, not WAVE",
            fourcc(&head[8..12])
        )));
    }
    let mut warnings = Vec::new();
    let mut fmt: Option<Fmt> = None;
    let mut data: Option<(u64, u64)> = None;
    let mut ds64_data: Option<u64> = None;
    let mut pos = 12u64;
    while pos + 8 <= file_len {
        r.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 8];
        r.read_exact(&mut h)?;
        let id = [h[0], h[1], h[2], h[3]];
        let mut size = u32_at(&h, 4) as u64;
        let body = pos + 8;
        if !fourcc_ok(&id) {
            if fmt.is_some() && data.is_some() {
                warnings.push(format!(
                    "ignored {} bytes of non-chunk data at the end",
                    file_len - pos
                ));
                break;
            }
            return Err(WavError::BadChunk(format!(
                "unreadable chunk id {} at byte {pos}",
                fourcc(&id)
            )));
        }
        let avail = file_len - body;
        match &id {
            b"ds64" => {
                if size < 24 || size > avail {
                    return Err(WavError::BadChunk(format!(
                        "ds64 chunk of {size} bytes (needs 24)"
                    )));
                }
                let mut b = [0u8; 24];
                r.read_exact(&mut b)?;
                ds64_data = Some(u64_at(&b, 8));
            }
            b"fmt " => {
                if size < 16 {
                    return Err(WavError::BadChunk(format!(
                        "fmt chunk is {size} bytes; it needs at least 16"
                    )));
                }
                if size > avail {
                    return Err(WavError::Truncated(format!(
                        "fmt chunk declares {size} bytes but only {avail} remain"
                    )));
                }
                let mut b = vec![0u8; size.min(64) as usize];
                r.read_exact(&mut b)?;
                fmt = Some(parse_fmt(&b)?);
            }
            b"data" => {
                if rf64 && size == 0xFFFF_FFFF {
                    size = ds64_data.ok_or_else(|| {
                        WavError::BadChunk("RF64 file without a ds64 chunk".into())
                    })?;
                }
                if size > avail {
                    return Err(WavError::Truncated(format!(
                        "data chunk declares {size} bytes but only {avail} bytes follow (the file is cut short)"
                    )));
                }
                if size == 0 && avail > 0 {
                    warnings.push(
                        "data chunk size is 0 (an unfinished recording?); read as empty".into(),
                    );
                }
                data = Some((body, size));
                if fmt.is_some() {
                    break;
                }
            }
            _ => {
                if size > avail {
                    if fmt.is_some() && data.is_some() {
                        warnings.push(format!(
                            "chunk '{}' after the audio is cut short (ignored)",
                            fourcc(&id)
                        ));
                        break;
                    }
                    return Err(WavError::Truncated(format!(
                        "chunk '{}' declares {size} bytes but only {avail} remain (the file is cut short)",
                        fourcc(&id)
                    )));
                }
            }
        }
        let mut next = body + size + (size & 1);
        if size & 1 == 1
            && !plausible_chunk(r, next, file_len)
            && plausible_chunk(r, body + size, file_len)
        {
            warnings.push(format!(
                "chunk '{}' has an odd size ({size}) and no pad byte; read anyway",
                fourcc(&id)
            ));
            next = body + size;
        }
        pos = next;
    }
    let fmt = fmt.ok_or_else(|| WavError::BadChunk("no fmt chunk".into()))?;
    let (data_offset, data_size) =
        data.ok_or_else(|| WavError::BadChunk("no data chunk (no audio in the file)".into()))?;
    let ba = fmt.block_align as u64;
    let rest = data_size % ba;
    if rest != 0 {
        warnings.push(format!(
            "data chunk ends with {rest} bytes of a partial frame (ignored)"
        ));
    }
    Ok(WavInfo {
        sample_rate: fmt.sample_rate,
        channels: fmt.channels,
        format: fmt.format,
        bits: fmt.bits,
        valid_bits: fmt.valid_bits,
        block_align: fmt.block_align,
        channel_mask: fmt.mask,
        frames: data_size / ba,
        data_offset,
        rf64,
        warnings,
    })
}

/// Streaming reader. `read_frames` hands out interleaved f64 samples in [-1, 1) (float files may
/// exceed that). NaN and infinite samples are replaced by 0 and counted in `nonfinite`.
pub struct WavReader<R: Read + Seek> {
    info: WavInfo,
    r: R,
    left: u64,
    raw: Vec<u8>,
    pub nonfinite: u64,
}

impl WavReader<BufReader<File>> {
    pub fn open(path: &Path) -> Result<Self, WavError> {
        // Look before opening: opening a pipe or a device waits for ever.
        let md = std::fs::metadata(path)
            .map_err(|e| WavError::Io(format!("{}: {e}", path.display())))?;
        if md.is_dir() {
            return Err(WavError::Io(format!("{} is a folder", path.display())));
        }
        if !md.is_file() {
            return Err(WavError::Io(format!(
                "{} is not a regular file (a pipe or device?)",
                path.display()
            )));
        }
        let f = File::open(path).map_err(|e| WavError::Io(format!("{}: {e}", path.display())))?;
        WavReader::new(BufReader::with_capacity(1 << 16, f))
    }
}

impl<R: Read + Seek> WavReader<R> {
    pub fn new(mut r: R) -> Result<Self, WavError> {
        let len = r.seek(SeekFrom::End(0))?;
        r.seek(SeekFrom::Start(0))?;
        let info = parse_header(&mut r, len)?;
        r.seek(SeekFrom::Start(info.data_offset))?;
        Ok(WavReader {
            left: info.frames,
            info,
            r,
            raw: Vec::new(),
            nonfinite: 0,
        })
    }

    pub fn info(&self) -> &WavInfo {
        &self.info
    }

    /// Up to `max_frames` frames, interleaved, into `out` (cleared first). Returns the number of
    /// frames read; 0 at the end.
    pub fn read_frames(
        &mut self,
        out: &mut Vec<f64>,
        max_frames: usize,
    ) -> Result<usize, WavError> {
        out.clear();
        let n = (max_frames as u64).min(self.left) as usize;
        if n == 0 {
            return Ok(0);
        }
        let ba = self.info.block_align as usize;
        self.raw.resize(n * ba, 0);
        self.r.read_exact(&mut self.raw).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                WavError::Truncated(
                    "the audio data ends early (the file changed while reading?)".into(),
                )
            } else {
                WavError::Io(e.to_string())
            }
        })?;
        self.left -= n as u64;
        out.reserve(n * self.info.channels as usize);
        let raw = &self.raw;
        match (self.info.format, self.info.bits) {
            (SampleFormat::Int, 8) => out.extend(raw.iter().map(|&b| (b as f64 - 128.0) / 128.0)),
            (SampleFormat::Int, 16) => out.extend(
                raw.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| i16::from_le_bytes(*c) as f64 / 32_768.0),
            ),
            (SampleFormat::Int, 24) => out.extend(raw.as_chunks::<3>().0.iter().map(|c| {
                let v = (c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i8 as i32) << 16);
                v as f64 / 8_388_608.0
            })),
            (SampleFormat::Int, 32) => out.extend(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| i32::from_le_bytes(*c) as f64 / 2_147_483_648.0),
            ),
            (SampleFormat::Float, 32) => out.extend(
                raw.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c) as f64),
            ),
            (SampleFormat::Float, 64) => out.extend(
                raw.as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| f64::from_le_bytes(*c)),
            ),
            (f, b) => {
                return Err(WavError::Unsupported(format!("{b}-bit {f:?}")));
            }
        }
        if self.info.format == SampleFormat::Float {
            for x in out.iter_mut() {
                if !x.is_finite() {
                    *x = 0.0;
                    self.nonfinite += 1;
                }
            }
        }
        Ok(n)
    }
}

/// Output encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutFormat {
    Pcm24,
    Float32,
}

impl OutFormat {
    pub fn parse(s: &str) -> Result<OutFormat, String> {
        match s.to_ascii_lowercase().as_str() {
            "pcm24" | "24" | "int24" => Ok(OutFormat::Pcm24),
            "float32" | "float" | "32f" => Ok(OutFormat::Float32),
            other => Err(format!("unknown output format {other} (pcm24 or float32)")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            OutFormat::Pcm24 => "pcm24",
            OutFormat::Float32 => "float32",
        }
    }

    fn bytes(self) -> u16 {
        match self {
            OutFormat::Pcm24 => 3,
            OutFormat::Float32 => 4,
        }
    }
}

/// Speaker mask for WAVE_FORMAT_EXTENSIBLE (only used above 2 channels).
fn default_mask(ch: u16) -> u32 {
    match ch {
        3 => 0x7,
        4 => 0x33,
        5 => 0x37,
        6 => 0x3F,
        8 => 0x63F,
        _ => 0,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WriteStats {
    pub frames: u64,
    /// Samples beyond full scale. 24-bit output clamps them; float output keeps them.
    pub overs: u64,
}

/// Writes a WAV to a file the caller has just created. Sizes are patched in `finish`.
pub struct WavWriter {
    w: BufWriter<File>,
    channels: u16,
    format: OutFormat,
    data_bytes: u64,
    header_len: u64,
    data_size_pos: u64,
    fact_pos: Option<u64>,
    overs: u64,
    buf: Vec<u8>,
}

const MAX_RIFF: u64 = 0xFFFF_FFFF;

impl WavWriter {
    pub fn new(
        file: File,
        sample_rate: u32,
        channels: u16,
        format: OutFormat,
    ) -> Result<Self, WavError> {
        if channels == 0 || sample_rate == 0 {
            return Err(WavError::BadChunk(format!(
                "cannot write {channels} channels at {sample_rate} Hz"
            )));
        }
        let bytes = format.bytes();
        let bits = bytes * 8;
        let block_align = channels * bytes;
        let extensible = channels > 2;
        let tag: u16 = match (extensible, format) {
            (true, _) => 0xFFFE,
            (false, OutFormat::Pcm24) => 1,
            (false, OutFormat::Float32) => 3,
        };
        let mut h: Vec<u8> = Vec::with_capacity(80);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        h.extend_from_slice(b"fmt ");
        h.extend_from_slice(&(if extensible { 40u32 } else { 16u32 }).to_le_bytes());
        h.extend_from_slice(&tag.to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&(sample_rate * block_align as u32).to_le_bytes());
        h.extend_from_slice(&block_align.to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        if extensible {
            h.extend_from_slice(&22u16.to_le_bytes());
            h.extend_from_slice(&bits.to_le_bytes());
            h.extend_from_slice(&default_mask(channels).to_le_bytes());
            let sub: u16 = if format == OutFormat::Pcm24 { 1 } else { 3 };
            h.extend_from_slice(&sub.to_le_bytes());
            h.extend_from_slice(&GUID_TAIL);
        }
        let mut fact_pos = None;
        if format == OutFormat::Float32 {
            h.extend_from_slice(b"fact");
            h.extend_from_slice(&4u32.to_le_bytes());
            fact_pos = Some(h.len() as u64);
            h.extend_from_slice(&0u32.to_le_bytes());
        }
        h.extend_from_slice(b"data");
        let data_size_pos = h.len() as u64;
        h.extend_from_slice(&0u32.to_le_bytes());
        let mut w = BufWriter::with_capacity(1 << 16, file);
        w.write_all(&h)?;
        Ok(WavWriter {
            w,
            channels,
            format,
            data_bytes: 0,
            header_len: h.len() as u64,
            data_size_pos,
            fact_pos,
            overs: 0,
            buf: Vec::new(),
        })
    }

    /// Interleaved samples; the count must be a whole number of frames.
    pub fn write(&mut self, interleaved: &[f64]) -> Result<(), WavError> {
        let ch = self.channels as usize;
        if !interleaved.len().is_multiple_of(ch) {
            return Err(WavError::BadChunk(format!(
                "{} samples is not a whole number of {ch}-channel frames",
                interleaved.len()
            )));
        }
        let add = interleaved.len() as u64 * self.format.bytes() as u64;
        if self.header_len + self.data_bytes + add > MAX_RIFF - 8 {
            return Err(WavError::TooLarge(
                "the output would pass the 4 GB WAV limit; split the job or use a lower sample rate"
                    .into(),
            ));
        }
        self.buf.clear();
        match self.format {
            OutFormat::Pcm24 => {
                for &x in interleaved {
                    let x = if x.is_finite() { x } else { 0.0 };
                    if x.abs() > 1.0 + 1e-9 {
                        self.overs += 1;
                    }
                    let v = (x * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                    self.buf.extend_from_slice(&v.to_le_bytes()[0..3]);
                }
            }
            OutFormat::Float32 => {
                for &x in interleaved {
                    let x = if x.is_finite() { x } else { 0.0 };
                    if x.abs() > 1.0 + 1e-9 {
                        self.overs += 1;
                    }
                    self.buf.extend_from_slice(&(x as f32).to_le_bytes());
                }
            }
        }
        self.w.write_all(&self.buf)?;
        self.data_bytes += add;
        Ok(())
    }

    /// Patch the sizes, flush and sync. The file is complete after this returns.
    pub fn finish(mut self) -> Result<WriteStats, WavError> {
        if self.data_bytes % 2 == 1 {
            self.w.write_all(&[0])?;
        }
        let frames = self.data_bytes / (self.channels as u64 * self.format.bytes() as u64);
        let riff = self.header_len - 8 + self.data_bytes + (self.data_bytes & 1);
        let mut f = self
            .w
            .into_inner()
            .map_err(|e| WavError::Io(e.to_string()))?;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(riff as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(self.data_size_pos))?;
        f.write_all(&(self.data_bytes as u32).to_le_bytes())?;
        if let Some(p) = self.fact_pos {
            f.seek(SeekFrom::Start(p))?;
            f.write_all(&(frames as u32).to_le_bytes())?;
        }
        f.sync_all()?;
        Ok(WriteStats {
            frames,
            overs: self.overs,
        })
    }
}

/// Whole-file convenience for tests and small files: (info, interleaved samples).
pub fn read_all(path: &Path) -> Result<(WavInfo, Vec<f64>), WavError> {
    let mut r = WavReader::open(path)?;
    let mut all = Vec::new();
    let mut block = Vec::new();
    while r.read_frames(&mut block, 1 << 16)? > 0 {
        all.extend_from_slice(&block);
    }
    Ok((r.info().clone(), all))
}

/// Write interleaved samples to a brand-new file at `path` (refuses an existing file).
pub fn write_new(
    path: &Path,
    sample_rate: u32,
    channels: u16,
    format: OutFormat,
    interleaved: &[f64],
) -> Result<WriteStats, WavError> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| WavError::Io(format!("{}: {e}", path.display())))?;
    let mut w = WavWriter::new(f, sample_rate, channels, format)?;
    w.write(interleaved)?;
    w.finish()
}

#[cfg(test)]
pub mod testwav {
    //! Hand-built WAV byte images for reader tests.

    pub fn fmt_chunk(tag: u16, ch: u16, sr: u32, bits: u16) -> Vec<u8> {
        let ba = ch * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&sr.to_le_bytes());
        b.extend_from_slice(&(sr * ba as u32).to_le_bytes());
        b.extend_from_slice(&ba.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b
    }

    pub fn ext_fmt_chunk(sub: u16, ch: u16, sr: u32, bits: u16, valid: u16, mask: u32) -> Vec<u8> {
        let ba = ch * bits / 8;
        let mut b = Vec::new();
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&0xFFFEu16.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&sr.to_le_bytes());
        b.extend_from_slice(&(sr * ba as u32).to_le_bytes());
        b.extend_from_slice(&ba.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(&22u16.to_le_bytes());
        b.extend_from_slice(&valid.to_le_bytes());
        b.extend_from_slice(&mask.to_le_bytes());
        b.extend_from_slice(&sub.to_le_bytes());
        b.extend_from_slice(&super::GUID_TAIL);
        b
    }

    pub fn chunk(id: &[u8; 4], body: &[u8], pad: bool) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(id);
        b.extend_from_slice(&(body.len() as u32).to_le_bytes());
        b.extend_from_slice(body);
        if pad && body.len() % 2 == 1 {
            b.push(0);
        }
        b
    }

    pub fn riff(parts: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = parts.concat();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(&body);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::testwav::*;
    use super::*;
    use crate::util::testutil::tmpdir;
    use std::io::Cursor;

    fn read_bytes(b: Vec<u8>) -> Result<(WavInfo, Vec<f64>), WavError> {
        let mut r = WavReader::new(Cursor::new(b))?;
        let mut all = Vec::new();
        let mut blk = Vec::new();
        while r.read_frames(&mut blk, 3)? > 0 {
            all.extend_from_slice(&blk);
        }
        Ok((r.info().clone(), all))
    }

    #[test]
    fn reads_every_integer_and_float_encoding() {
        // 16-bit stereo
        let mut d = Vec::new();
        for v in [16384i16, -16384, 32767, -32768] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        let (i, s) = read_bytes(riff(&[
            fmt_chunk(1, 2, 44100, 16),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!(
            (i.channels, i.sample_rate, i.frames, i.bits),
            (2, 44100, 2, 16)
        );
        assert_eq!(s, vec![0.5, -0.5, 32767.0 / 32768.0, -1.0]);
        // 24-bit mono, including the sign extension
        let mut d = Vec::new();
        for v in [4_194_304i32, -4_194_304, -1] {
            d.extend_from_slice(&v.to_le_bytes()[0..3]);
        }
        let (_, s) = read_bytes(riff(&[
            fmt_chunk(1, 1, 48000, 24),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!(s, vec![0.5, -0.5, -1.0 / 8_388_608.0]);
        // 32-bit int
        let d: Vec<u8> = [1_073_741_824i32, i32::MIN]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (_, s) = read_bytes(riff(&[
            fmt_chunk(1, 1, 48000, 32),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!(s, vec![0.5, -1.0]);
        // 8-bit unsigned
        let (_, s) = read_bytes(riff(&[
            fmt_chunk(1, 1, 8000, 8),
            chunk(b"data", &[128, 192, 0], true),
        ]))
        .unwrap();
        assert_eq!(s, vec![0.0, 0.5, -1.0]);
        // float 32 and 64; NaN is replaced and counted
        let d: Vec<u8> = [0.25f32, -2.0, f32::NAN]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut r = WavReader::new(Cursor::new(riff(&[
            fmt_chunk(3, 1, 48000, 32),
            chunk(b"data", &d, true),
        ])))
        .unwrap();
        let mut blk = Vec::new();
        r.read_frames(&mut blk, 10).unwrap();
        assert_eq!(blk, vec![0.25, -2.0, 0.0]);
        assert_eq!(r.nonfinite, 1);
        let d: Vec<u8> = [0.125f64, -0.75]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let (i, s) = read_bytes(riff(&[
            fmt_chunk(3, 2, 96000, 64),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!((i.format, i.frames), (SampleFormat::Float, 1));
        assert_eq!(s, vec![0.125, -0.75]);
    }

    #[test]
    fn reads_extensible_with_mask_and_many_channels() {
        let ch = 6u16;
        let d: Vec<u8> = (0..ch * 2)
            .flat_map(|k| ((k as i16) * 1000).to_le_bytes())
            .collect();
        let (i, s) = read_bytes(riff(&[
            ext_fmt_chunk(1, ch, 48000, 16, 16, 0x3F),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!((i.channels, i.channel_mask, i.frames), (6, Some(0x3F), 2));
        assert_eq!(s.len(), 12);
        assert!((s[11] - 11000.0 / 32768.0).abs() < 1e-12);
        // 20 valid bits in a 24-bit container read as 24-bit.
        let d = [0u8, 0, 0x40];
        let (i, s) = read_bytes(riff(&[
            ext_fmt_chunk(1, 1, 48000, 24, 20, 0x4),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!((i.valid_bits, i.bits), (20, 24));
        assert_eq!(s, vec![0.5]);
        // Extensible float
        let d: Vec<u8> = 0.5f32.to_le_bytes().to_vec();
        let (i, _) = read_bytes(riff(&[
            ext_fmt_chunk(3, 1, 48000, 32, 32, 0),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!(i.format, SampleFormat::Float);
        // 32 channels
        let d = vec![0u8; 32 * 2 * 4];
        let (i, _) = read_bytes(riff(&[
            fmt_chunk(1, 32, 48000, 16),
            chunk(b"data", &d, true),
        ]))
        .unwrap();
        assert_eq!((i.channels, i.frames), (32, 4));
    }

    #[test]
    fn skips_other_chunks_handles_odd_sizes_and_fmt_after_data() {
        let d: Vec<u8> = [8192i16, -8192]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        // LIST with an odd size and a proper pad byte, before fmt.
        let b = riff(&[
            chunk(b"LIST", b"INFOabc", true),
            fmt_chunk(1, 1, 48000, 16),
            chunk(b"data", &d, true),
        ]);
        let (i, s) = read_bytes(b).unwrap();
        assert_eq!(s, vec![0.25, -0.25]);
        assert!(i.warnings.is_empty(), "{:?}", i.warnings);
        // The same, but the writer forgot the pad byte: read anyway, with a warning.
        let b = riff(&[
            chunk(b"LIST", b"INFOabc", false),
            fmt_chunk(1, 1, 48000, 16),
            chunk(b"data", &d, true),
        ]);
        let (i, s) = read_bytes(b).unwrap();
        assert_eq!(s, vec![0.25, -0.25]);
        assert!(i.warnings[0].contains("no pad byte"), "{:?}", i.warnings);
        // fmt after data.
        let b = riff(&[chunk(b"data", &d, true), fmt_chunk(1, 1, 48000, 16)]);
        assert_eq!(read_bytes(b).unwrap().1, vec![0.25, -0.25]);
        // A partial last frame is ignored with a warning.
        let mut d3 = d.clone();
        d3.push(7);
        let b = riff(&[fmt_chunk(1, 1, 48000, 16), chunk(b"data", &d3, true)]);
        let (i, s) = read_bytes(b).unwrap();
        assert_eq!(s.len(), 2);
        assert!(i.warnings.iter().any(|w| w.contains("partial frame")));
    }

    #[test]
    fn clear_errors_for_unsupported_truncated_and_broken() {
        let err = |b: Vec<u8>| read_bytes(b).unwrap_err();
        let d = vec![0u8; 8];
        // ADPCM
        let e = err(riff(&[
            fmt_chunk(2, 1, 48000, 16),
            chunk(b"data", &d, true),
        ]));
        assert!(
            matches!(e, WavError::Unsupported(_)) && e.to_string().contains("ADPCM"),
            "{e}"
        );
        // 12-bit integer, 16-bit float
        assert!(matches!(
            err(riff(&[
                fmt_chunk(1, 1, 48000, 12),
                chunk(b"data", &d, true)
            ])),
            WavError::Unsupported(_) | WavError::BadChunk(_)
        ));
        assert!(matches!(
            err(riff(&[
                fmt_chunk(3, 1, 48000, 16),
                chunk(b"data", &d, true)
            ])),
            WavError::Unsupported(_)
        ));
        // Truncated data chunk.
        let mut b = riff(&[
            fmt_chunk(1, 1, 48000, 16),
            chunk(b"data", &[0u8; 100], true),
        ]);
        b.truncate(b.len() - 40);
        let e = err(b);
        assert!(
            matches!(e, WavError::Truncated(_)) && e.to_string().contains("cut short"),
            "{e}"
        );
        // Truncated before the data chunk.
        let mut b = riff(&[
            fmt_chunk(1, 1, 48000, 16),
            chunk(b"LIST", &[1u8; 50], true),
            chunk(b"data", &d, true),
        ]);
        b.truncate(60);
        assert!(matches!(err(b), WavError::Truncated(_)));
        // Tiny file, not RIFF, other containers.
        assert!(matches!(err(b"RIFF".to_vec()), WavError::Truncated(_)));
        assert!(err(b"FORM\0\0\0\0AIFFxxxx".to_vec())
            .to_string()
            .contains("AIFF"));
        assert!(err(b"ID3\x04\0\0\0\0\0\0\0\0".to_vec())
            .to_string()
            .contains("MP3"));
        assert!(matches!(
            err(b"RIFX\0\0\0\0WAVE".to_vec()),
            WavError::Unsupported(_)
        ));
        assert!(matches!(
            err(b"RIFF\0\0\0\0AVI ".to_vec()),
            WavError::NotWav(_)
        ));
        // Missing fmt, missing data, short fmt, bad block align, zero channels.
        assert!(err(riff(&[chunk(b"data", &d, true)]))
            .to_string()
            .contains("no fmt"));
        assert!(err(riff(&[fmt_chunk(1, 1, 48000, 16)]))
            .to_string()
            .contains("no data"));
        assert!(err(riff(&[
            chunk(b"fmt ", &[1, 0, 1, 0], true),
            chunk(b"data", &d, true)
        ]))
        .to_string()
        .contains("at least 16"));
        let mut f = fmt_chunk(1, 2, 48000, 16);
        f[20] = 3; // block align 3 for 2 x 16-bit
        assert!(err(riff(&[f, chunk(b"data", &d, true)]))
            .to_string()
            .contains("block align"));
        assert!(matches!(
            err(riff(&[
                fmt_chunk(1, 0, 48000, 16),
                chunk(b"data", &d, true)
            ])),
            WavError::BadChunk(_)
        ));
        // Garbage where a chunk header should be.
        let mut b = riff(&[fmt_chunk(1, 1, 48000, 16)]);
        b.extend_from_slice(&[0xff, 0, 1, 2, 3, 4, 5, 6]);
        assert!(matches!(err(b), WavError::BadChunk(_)));
    }

    #[test]
    fn rf64_with_ds64_is_read() {
        let d: Vec<u8> = [16384i16, -16384]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut ds = Vec::new();
        ds.extend_from_slice(&0u64.to_le_bytes());
        ds.extend_from_slice(&(d.len() as u64).to_le_bytes());
        ds.extend_from_slice(&2u64.to_le_bytes());
        ds.extend_from_slice(&0u32.to_le_bytes());
        let mut b = riff(&[chunk(b"ds64", &ds, true), fmt_chunk(1, 1, 48000, 16)]);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        b.extend_from_slice(&d);
        b[0..4].copy_from_slice(b"RF64");
        let (i, s) = read_bytes(b).unwrap();
        assert!(i.rf64);
        assert_eq!(s, vec![0.5, -0.5]);
    }

    #[test]
    fn writer_round_trips_24_bit_and_float_and_never_overwrites() {
        let d = tmpdir("wavw");
        let sig: Vec<f64> = (0..2000).map(|n| 0.9 * ((n as f64) * 0.05).sin()).collect();
        for (fmt, ch, tol) in [
            (OutFormat::Pcm24, 2u16, 1.0 / 8_388_608.0),
            (OutFormat::Float32, 2, 1e-7),
            (OutFormat::Pcm24, 6, 1.0 / 8_388_608.0),
            (OutFormat::Float32, 1, 1e-7),
        ] {
            let p = d.join(format!("rt-{}-{ch}.wav", fmt.name()));
            let st = write_new(
                &p,
                48000,
                ch,
                fmt,
                &sig[..(sig.len() / ch as usize) * ch as usize],
            )
            .unwrap();
            let (i, back) = read_all(&p).unwrap();
            assert_eq!(st.frames, i.frames);
            assert_eq!(i.channels, ch);
            assert_eq!(i.sample_rate, 48000);
            assert!(i.warnings.is_empty(), "{:?}", i.warnings);
            for (a, b) in back.iter().zip(&sig) {
                assert!((a - b).abs() <= tol, "{fmt:?} {a} {b}");
            }
            if ch > 2 {
                assert_eq!(i.channel_mask, Some(0x3F));
            }
            // The file is refused a second time: writers never overwrite.
            assert!(write_new(&p, 48000, ch, fmt, &[]).is_err());
        }
        // Odd data size (1 channel of 24-bit, 1 frame) gets a pad byte and still reads.
        let p = d.join("odd.wav");
        write_new(&p, 44100, 1, OutFormat::Pcm24, &[0.5]).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len() % 2, 0);
        assert_eq!(read_all(&p).unwrap().1, vec![0.5]);
    }

    #[test]
    fn writer_clamps_overs_in_24_bit_and_counts_them() {
        let d = tmpdir("overs");
        let p = d.join("o.wav");
        let st = write_new(
            &p,
            48000,
            1,
            OutFormat::Pcm24,
            &[1.5, -1.5, 1.0, f64::NAN, 0.25],
        )
        .unwrap();
        assert_eq!(st.overs, 2);
        let (_, s) = read_all(&p).unwrap();
        assert_eq!(s[0], 8_388_607.0 / 8_388_608.0);
        assert_eq!(s[1], -1.0);
        assert_eq!(s[3], 0.0);
        assert_eq!(s[4], 0.25);
        let p = d.join("f.wav");
        let st = write_new(&p, 48000, 1, OutFormat::Float32, &[1.5, 0.5]).unwrap();
        assert_eq!(st.overs, 1);
        assert_eq!(read_all(&p).unwrap().1, vec![1.5, 0.5]);
    }

    #[test]
    fn streaming_reads_in_blocks_with_constant_buffer() {
        let d = tmpdir("stream");
        let p = d.join("long.wav");
        let n = 48000 * 3;
        // Multiples of 1/256 are exact in f32, so the float round trip is exact.
        let sig: Vec<f64> = (0..n * 2)
            .map(|k| ((k % 256) as f64 / 256.0) - 0.5)
            .collect();
        write_new(&p, 48000, 2, OutFormat::Float32, &sig).unwrap();
        let mut r = WavReader::open(&p).unwrap();
        let mut blk = Vec::new();
        let mut total = 0;
        let mut max_len = 0;
        loop {
            let got = r.read_frames(&mut blk, 4096).unwrap();
            if got == 0 {
                break;
            }
            max_len = max_len.max(blk.len());
            assert_eq!(blk[..], sig[total * 2..(total + got) * 2]);
            total += got;
        }
        assert_eq!(total, n);
        assert_eq!(max_len, 4096 * 2);
        assert!(WavReader::open(&d).is_err());
        let e = WavReader::open(&d.join("missing.wav")).err().unwrap();
        assert!(e.to_string().contains("missing.wav"), "{e}");
    }
}
