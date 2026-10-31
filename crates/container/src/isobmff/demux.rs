//! Demuxer: parses box structure lazily (headers + `moov`/`moof` only) and builds per-track sample tables.

use crate::isobmff::bytes::{BoxHeader, Cur, FourCc, boxes, find};
use crate::isobmff::codec::{CodecConfig, SampleEntry, parse_sample_entry};
use crate::isobmff::error::{Error, Result, invalid};
use crate::isobmff::source::ByteSource;
use std::collections::HashMap;

/// Hard cap on the sample count of the whole file, all tracks and fragments (after PCM chunk merging).
const MAX_SAMPLES: usize = 1 << 26;
/// Bytes of file each sample needs beyond the first `MIN_SAMPLE_ALLOWANCE` samples. A [`Sample`]
/// takes 40 bytes of memory and a constant-size `stsz` or a `trun` without per-sample fields
/// describes one in a byte or none, so without this a file could make the sample tables about
/// 40 times its own size. A table-backed sample has a 4-byte `stsz` or `trun` entry.
const FILE_BYTES_PER_SAMPLE: u64 = 4;
/// Samples any file may hold, however short: 40 MiB of sample tables.
const MIN_SAMPLE_ALLOWANCE: usize = 1 << 20;
/// Hard cap on the size of an in-memory `moov`/`moof` box.
const MAX_HEADER_BOX: u64 = 1 << 30;
/// Most text all metadata entries may hold together; entries past it are dropped. One `keys`
/// name can be named by many `ilst` items, each a copy, so the file's size does not bound this.
const MAX_METADATA_BYTES: usize = 4 << 20;
/// What an entry costs beyond its text, so that empty entries are bounded too.
const METADATA_ENTRY_COST: usize = 64;

/// Largest sample built from a PCM chunk; larger chunks are split. Writers that put a whole
/// recording in one chunk would otherwise give samples past `u32::MAX` bytes.
const PCM_MAX_SAMPLE_BYTES: u64 = 1 << 24;

/// Frames in one piece of a split PCM chunk: whole units, at most `PCM_MAX_SAMPLE_BYTES`.
fn pcm_piece_frames(unit: u64, unit_bytes: u64) -> u64 {
    (PCM_MAX_SAMPLE_BYTES / unit_bytes.max(1)).max(1) * unit
}

/// What is left of the file-wide limits while tracks and fragments are parsed.
struct SampleBudget {
    /// Samples (what the file's length allows, at most `MAX_SAMPLES`, minus those already built).
    samples: usize,
    /// Bytes of file for samples whose count no table entry backs (constant-size `stsz`, `trun`
    /// without per-sample fields). Each such sample claims at least one byte, so a file can't
    /// describe more of them than it has bytes, however many tracks or fragments repeat the claim.
    implied_bytes: u64,
}

impl SampleBudget {
    fn new(file_len: u64) -> Self {
        let by_len = usize::try_from(file_len / FILE_BYTES_PER_SAMPLE).unwrap_or(usize::MAX);
        Self { samples: by_len.clamp(MIN_SAMPLE_ALLOWANCE, MAX_SAMPLES), implied_bytes: file_len }
    }

    fn take(&mut self, n: usize) -> Result<()> {
        self.samples = self.samples.checked_sub(n).ok_or(Error::TooLarge("sample count"))?;
        Ok(())
    }
}

/// One sample (access unit) of a track, in the track's media timescale.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Sample {
    /// Absolute byte offset in the file.
    pub offset: u64,
    pub size: u32,
    /// Decode timestamp (raw media time, edit lists not applied).
    pub dts: i64,
    /// Presentation timestamp = dts + composition offset (raw media time).
    pub pts: i64,
    pub duration: u32,
    pub is_sync: bool,
    /// 1-based index into [`Track::entries`].
    pub description_index: u32,
}

/// Compact sample storage. Samples are reconstructed by value with [`Self::get`] or
/// [`Self::iter`], rather than keeping a full `Sample` and two indexes per access unit.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SampleTable {
    /// The append cursor avoids reconstructing up to 255 offsets for each parsed sample.
    last_sample: Option<Sample>,
    sizes: Vec<u32>,
    /// Nonzero gaps from the preceding sample's end, with one presence bit per sample.
    gaps: Vec<u32>,
    gap_marks: Vec<u64>,
    chunks: Vec<SampleChunk>,
    times: Vec<SampleTime>,
    presentation: Vec<PresentationBlock>,
    /// Empty means no composition offsets (the usual audio and intra-only case).
    composition: Vec<i32>,
    /// Only fragmented files can carry composition offsets outside signed 32-bit range.
    wide_composition: Vec<i64>,
    /// None means every sample is sync.
    sync: Option<Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq)]
struct SampleChunk {
    first: u32,
    description: u32,
    offset: u64,
    gap_start: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct SampleTime {
    first: u32,
    duration: u32,
    dts: i64,
}

/// Bounds for 256 samples in decode order, replacing the full PTS permutation.
#[derive(Clone, Debug, PartialEq)]
struct PresentationBlock {
    min: i64,
    max: i64,
    first: u32,
    last: u32,
}

impl SampleTable {
    pub fn len(&self) -> usize {
        self.sizes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<Sample> {
        let size = *self.sizes.get(index)?;
        let chunk = self.chunks.get(self.chunks.partition_point(|c| c.first as usize <= index).checked_sub(1)?)?;
        let (dts, pts, duration) = self.timing(index)?;
        let mut offset = chunk.offset;
        let mut gap_index = chunk.gap_start;
        for (i, &size) in self.sizes.get(chunk.first as usize..index)?.iter().enumerate() {
            let next = chunk.first as usize + i + 1;
            offset = offset.saturating_add(size as u64);
            if self.gap_marks.get(next / 64).copied().unwrap_or(0) & (1 << (next % 64)) != 0 {
                offset = offset.saturating_add(*self.gaps.get(gap_index)? as u64);
                gap_index += 1;
            }
        }
        Some(Sample {
            offset,
            size,
            dts,
            pts,
            duration,
            is_sync: self.sync.as_ref().is_none_or(|s| s.binary_search(&(index as u32)).is_ok()),
            description_index: chunk.description,
        })
    }

    fn timing(&self, index: usize) -> Option<(i64, i64, u32)> {
        if index >= self.len() {
            return None;
        }
        let time = self.times.get(self.times.partition_point(|t| t.first as usize <= index).checked_sub(1)?)?;
        let dts = time.dts.saturating_add(((index - time.first as usize) as i64).saturating_mul(time.duration as i64));
        let cto = self
            .wide_composition
            .get(index)
            .copied()
            .unwrap_or_else(|| self.composition.get(index).copied().unwrap_or(0) as i64);
        Some((dts, dts.saturating_add(cto), time.duration))
    }

    pub fn first(&self) -> Option<Sample> {
        self.get(0)
    }
    pub fn last(&self) -> Option<Sample> {
        self.last_sample
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Sample> + DoubleEndedIterator + '_ {
        self.into_iter()
    }

    fn reserve(&mut self, n: usize) {
        self.sizes.reserve(n);
    }
    fn try_reserve(&mut self, n: usize) -> std::result::Result<(), std::collections::TryReserveError> {
        self.sizes.try_reserve(n)
    }

    fn push(&mut self, s: Sample) {
        let index = self.len() as u32;
        let previous = self.last();
        let gap = previous
            .and_then(|p| s.offset.checked_sub(p.offset.saturating_add(p.size as u64)))
            .and_then(|gap| u32::try_from(gap).ok());
        if let Some(gap) = gap.filter(|&g| g != 0) {
            let word = index as usize / 64;
            self.gap_marks.resize(word + 1, 0);
            if let Some(bits) = self.gap_marks.get_mut(word) {
                *bits |= 1 << (index % 64);
            }
            self.gaps.push(gap);
        }
        // Bound offset reconstruction to 255 additions, even for a whole movie in one chunk.
        if index.is_multiple_of(256)
            || gap.is_none()
            || previous.is_none_or(|p| p.description_index != s.description_index)
        {
            self.chunks.push(SampleChunk {
                first: index,
                description: s.description_index,
                offset: s.offset,
                gap_start: self.gaps.len(),
            });
        }
        if previous.is_none_or(|p| p.dts.saturating_add(p.duration as i64) != s.dts || p.duration != s.duration) {
            self.times.push(SampleTime { first: index, duration: s.duration, dts: s.dts });
        }
        let cto = s.pts.saturating_sub(s.dts);
        if i32::try_from(cto).is_err() && self.wide_composition.is_empty() {
            self.wide_composition.reserve(self.sizes.capacity());
            self.wide_composition.extend(self.composition.iter().map(|&c| c as i64));
            self.wide_composition.resize(index as usize, 0);
            self.composition = Vec::new();
        }
        if !self.wide_composition.is_empty() || i32::try_from(cto).is_err() {
            self.wide_composition.push(cto);
        } else if cto != 0 || !self.composition.is_empty() {
            if self.composition.is_empty() {
                self.composition.reserve(self.sizes.capacity());
                self.composition.resize(index as usize, 0);
            }
            self.composition.push(cto as i32);
        }
        if !s.is_sync && self.sync.is_none() {
            self.sync = Some((0..index).collect());
        }
        if s.is_sync
            && let Some(sync) = &mut self.sync
        {
            sync.push(index);
        }
        if index.is_multiple_of(256) {
            self.presentation.push(PresentationBlock { min: s.pts, max: s.pts, first: index, last: index });
        } else if let Some(block) = self.presentation.last_mut() {
            if s.pts < block.min {
                block.min = s.pts;
                block.first = index;
            }
            if s.pts >= block.max {
                block.max = s.pts;
                block.last = index;
            }
        }
        self.sizes.push(s.size);
        self.last_sample = Some(s);
    }

    fn compact(&mut self) {
        self.sizes.shrink_to_fit();
        self.gaps.shrink_to_fit();
        self.gap_marks.shrink_to_fit();
        self.chunks.shrink_to_fit();
        self.times.shrink_to_fit();
        self.presentation.shrink_to_fit();
        self.composition.shrink_to_fit();
        self.wide_composition.shrink_to_fit();
        if let Some(sync) = &mut self.sync {
            sync.shrink_to_fit();
        }
    }
}

impl<'a> IntoIterator for &'a SampleTable {
    type Item = Sample;
    type IntoIter = SampleIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        SampleIter { table: self, range: 0..self.len() }
    }
}

pub struct SampleIter<'a> {
    table: &'a SampleTable,
    range: std::ops::Range<usize>,
}

impl Iterator for SampleIter<'_> {
    type Item = Sample;
    fn next(&mut self) -> Option<Sample> {
        self.table.get(self.range.next()?)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }
}
impl DoubleEndedIterator for SampleIter<'_> {
    fn next_back(&mut self) -> Option<Sample> {
        self.table.get(self.range.next_back()?)
    }
}
impl ExactSizeIterator for SampleIter<'_> {}

/// One edit list entry (`elst`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    /// Duration of this edit in the movie timescale.
    pub segment_duration: u64,
    /// Start time within the media (track timescale); `-1` marks an empty edit.
    pub media_time: i64,
    /// Media rate as 16.16 fixed point (0x0001_0000 = normal speed, 0 = dwell).
    pub media_rate: i32,
}

/// Broad classification of a track by its handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Timecode,
    Subtitle,
    Other,
}

/// A sample-to-group mapping (`sbgp`) with the matching descriptions (`sgpd`) kept raw.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SampleGroup {
    pub grouping_type: FourCc,
    /// (sample_count, group_description_index) runs.
    pub runs: Vec<(u32, u32)>,
    /// Raw description entries from `sgpd`.
    pub descriptions: Vec<Vec<u8>>,
}

/// A demuxed track.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    pub handler: FourCc,
    pub handler_name: String,
    /// Media timescale (`mdhd`).
    pub timescale: u32,
    /// Media duration in the media timescale (`mdhd`).
    pub duration: u64,
    /// Track duration in the movie timescale (`tkhd`).
    pub tkhd_duration: u64,
    /// ISO 639-2/T language code (e.g. `und`, `eng`).
    pub language: String,
    /// `tkhd` presentation size (16.16 fixed point converted to integer pixels, rounded down).
    pub width: u32,
    pub height: u32,
    pub enabled: bool,
    pub alternate_group: u16,
    /// `tkhd` volume (8.8 fixed point).
    pub volume: i16,
    /// `tkhd` transformation matrix.
    pub matrix: [i32; 9],
    pub entries: Vec<SampleEntry>,
    pub samples: SampleTable,
    pub edits: Vec<Edit>,
    /// Offset to add to raw pts to get presentation time (track timescale), derived from the
    /// leading empty edits and the first non-empty edit's `media_time`.
    pub edit_offset: i64,
    /// Track references (`tref`): (type, track ids).
    pub references: Vec<(FourCc, Vec<u32>)>,
    /// Raw `sdtp` bytes (one per sample) if present.
    pub sdtp: Vec<u8>,
    pub sample_groups: Vec<SampleGroup>,
    /// True if uncompressed audio samples were merged into one [`Sample`] per chunk.
    pub pcm_chunked: bool,
}

impl Track {
    /// The first sample entry's codec configuration.
    pub fn codec(&self) -> Option<&CodecConfig> {
        self.entries.first().map(|e| &e.codec)
    }
    pub fn video(&self) -> Option<&crate::isobmff::codec::VideoParams> {
        self.entries.first().and_then(|e| e.video.as_ref())
    }
    /// Clockwise quarter turns (0–3) the `tkhd` matrix applies for display, or None when it is not
    /// a pure 0/90/180/270° rotation (mirrored, scaled or sheared — left to the caller).
    ///
    /// ISO/IEC 14496-12 §8.3.2: a sample point (p, q) is displayed at p′ = a·p + c·q + x,
    /// q′ = b·p + d·q + y for the matrix {a, b, u, c, d, v, x, y, w}, with y pointing down. So
    /// {0, 1, −1, 0} sends the top row to the right-hand column (90° clockwise — iPhone portrait
    /// video), {−1, 0, 0, −1} is 180° and {0, −1, 1, 0} is 270°. Only the signs matter: a·d − b·c
    /// carries the scale, which the presentation size already reflects.
    pub fn display_rotation(&self) -> Option<u8> {
        let [a, b, _, c, d, _, _, _, _] = self.matrix;
        match (a.signum(), b.signum(), c.signum(), d.signum()) {
            (1, 0, 0, 1) => Some(0),
            (0, 1, -1, 0) => Some(1),
            (-1, 0, 0, -1) => Some(2),
            (0, -1, 1, 0) => Some(3),
            _ => None,
        }
    }
    pub fn audio(&self) -> Option<&crate::isobmff::codec::AudioParams> {
        self.entries.first().and_then(|e| e.audio.as_ref())
    }
    /// Presentation (edit-adjusted) pts of sample `i`.
    pub fn presentation_pts(&self, i: usize) -> Option<i64> {
        self.samples.get(i).map(|s| s.pts.saturating_add(self.edit_offset))
    }
    /// Index of the sample being presented at raw media time `pts` (the last sample, in
    /// presentation order, whose pts is ≤ `pts`; the first sample if `pts` precedes all).
    pub fn sample_at_pts(&self, pts: i64) -> Option<usize> {
        let (mut first, mut best) = (None, None);
        for (bi, block) in self.samples.presentation.iter().enumerate() {
            let low = (block.min, block.first as usize);
            if first.is_none_or(|f| low < f) {
                first = Some(low);
            }
            if block.max <= pts {
                let high = (block.max, block.last as usize);
                if best.is_none_or(|b| high > b) {
                    best = Some(high);
                }
            } else if block.min <= pts {
                for i in bi * 256..((bi + 1) * 256).min(self.samples.len()) {
                    let (_, time, _) = self.samples.timing(i)?;
                    let candidate = (time, i);
                    if time <= pts && best.is_none_or(|b| candidate > b) {
                        best = Some(candidate);
                    }
                }
            }
        }
        best.or(first).map(|(_, i)| i)
    }
    /// Like [`sample_at_pts`](Self::sample_at_pts) but for edit-adjusted presentation time.
    pub fn sample_at_presentation_time(&self, t: i64) -> Option<usize> {
        self.sample_at_pts(t.saturating_sub(self.edit_offset))
    }
    /// The closest sync sample at or before `index` (decode order); 0 if there is none.
    pub fn sync_sample_before(&self, index: usize) -> usize {
        let Some(sync) = &self.samples.sync else {
            return index.min(self.samples.len().saturating_sub(1));
        };
        let k = sync.partition_point(|&i| i as usize <= index);
        if k == 0 { 0 } else { sync[k - 1] as usize }
    }
    /// Indices of all sync samples.
    pub fn sync_samples(&self) -> impl Iterator<Item = u32> + '_ {
        self.samples
            .sync
            .iter()
            .flatten()
            .copied()
            .chain(0..if self.samples.sync.is_none() { self.samples.len() as u32 } else { 0 })
    }
}

/// Text metadata from `udta` (QuickTime `©xxx` atoms and iTunes-style `meta/ilst`, incl. `keys`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Metadata {
    pub entries: Vec<(String, String)>,
}

impl Metadata {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    /// Title from `©nam` or the `com.apple.quicktime.title` key.
    pub fn title(&self) -> Option<&str> {
        self.get("©nam").or_else(|| self.get("com.apple.quicktime.title")).or_else(|| self.get("title"))
    }
}

/// A parsed MP4/MOV file (headers and sample tables; media data stays in the source).
#[derive(Clone, Debug, PartialEq)]
pub struct Mp4File {
    pub major_brand: FourCc,
    pub minor_version: u32,
    pub compatible_brands: Vec<FourCc>,
    /// QuickTime file (major brand `qt  ` or no `ftyp`).
    pub is_quicktime: bool,
    /// Movie timescale (`mvhd`).
    pub timescale: u32,
    /// Movie duration in the movie timescale (`mvhd`; for fragmented files, the longest track).
    pub duration: u64,
    pub tracks: Vec<Track>,
    pub metadata: Metadata,
    /// File uses movie fragments (`mvex` present).
    pub fragmented: bool,
    /// Byte offset of the `moov` box.
    pub moov_offset: u64,
    /// Number of `moof` boxes found.
    pub fragment_count: usize,
}

impl Mp4File {
    /// Read the bytes of sample `index` of track `track` (index into [`tracks`](Self::tracks)).
    pub fn read_sample<S: ByteSource + ?Sized>(&self, src: &S, track: usize, index: usize) -> Result<Vec<u8>> {
        let t = self.tracks.get(track).ok_or(Error::NoSuchTrack(track))?;
        let s = t.samples.get(index).ok_or(Error::NoSuchSample(index))?;
        read_range(src, s.offset, s.size as u64)
    }
    pub fn sample_at_pts(&self, track: usize, pts: i64) -> Option<usize> {
        self.tracks.get(track)?.sample_at_pts(pts)
    }
    pub fn sync_sample_before(&self, track: usize, index: usize) -> Option<usize> {
        Some(self.tracks.get(track)?.sync_sample_before(index))
    }
    /// First track of the given kind.
    pub fn track_of_kind(&self, kind: TrackKind) -> Option<usize> {
        self.tracks.iter().position(|t| t.kind == kind)
    }
}

fn read_range<S: ByteSource + ?Sized>(src: &S, offset: u64, size: u64) -> Result<Vec<u8>> {
    let end = offset.checked_add(size).ok_or(Error::Truncated("sample"))?;
    if end > src.len() {
        return Err(Error::Truncated("sample data"));
    }
    let mut v = vec![0u8; size as usize];
    src.read_at(offset, &mut v)?;
    Ok(v)
}

/// Parse an MP4/MOV file. Only box headers, `ftyp`, `moov` and `moof` boxes are read.
pub fn open<S: ByteSource>(src: S) -> Result<Mp4File> {
    open_inner(src, None, false)
}

/// Open a progressive file for a bounded, indexed read. Refuse fragments before
/// loading them, and check caller budgets *before* allocating headers or tables.
pub fn open_progressive<S: ByteSource>(src: S, max_header: u64, max_samples: usize) -> Result<Mp4File> {
    open_inner(src, Some((max_header, max_samples)), false)
}

/// Parse a fragmented init segment (`ftyp` + `moov` with `mvex`, no `moof`) whose tracks have no
/// samples. Non-empty sample tables fail before their samples are built.
pub(crate) fn open_init(data: &[u8], max_header: u64) -> Result<Mp4File> {
    open_inner(data, Some((max_header, 0)), true)
}

fn open_inner<S: ByteSource>(src: S, limits: Option<(u64, usize)>, init: bool) -> Result<Mp4File> {
    let len = src.len();
    let mut offset = 0u64;
    let mut ftyp: Option<Vec<u8>> = None;
    let mut moov: Option<(u64, Vec<u8>)> = None;
    let mut moofs: Vec<(u64, Vec<u8>)> = Vec::new();
    let mut top_boxes = 0usize;
    while len.saturating_sub(offset) >= 8 {
        top_boxes += 1;
        if limits.is_some() && top_boxes > 4096 {
            return Err(Error::TooLarge("top-level box count"));
        }
        let n = (len - offset).min(32) as usize;
        let mut hb = [0u8; 32];
        src.read_at(offset, &mut hb[..n])?;
        let h = BoxHeader::parse(&hb[..n])?;
        let size = h.size.unwrap_or(len - offset);
        let end = offset.saturating_add(size).min(len);
        let body_len = end.saturating_sub(offset + h.header_len);
        let read_body = |max: u64| -> Result<Vec<u8>> {
            if body_len > max {
                return Err(Error::TooLarge("header box"));
            }
            read_range(&src, offset + h.header_len, body_len)
        };
        match &h.kind.0 {
            b"ftyp" if ftyp.is_none() => ftyp = Some(read_body(1 << 16)?),
            b"moov" if moov.is_none() => {
                moov =
                    Some((offset, read_body(limits.map_or(MAX_HEADER_BOX, |(header, _)| header.min(MAX_HEADER_BOX)))?))
            }
            b"moof" if limits.is_some() => return Err(Error::Unsupported("fragmented MP4".into())),
            b"moof" => moofs.push((offset, read_body(MAX_HEADER_BOX)?)),
            _ => {}
        }
        if h.size.is_none() {
            break;
        }
        // A box that claims to run past the end of the file ends the scan.
        offset = end;
    }
    let (moov_offset, moov) = moov.ok_or(Error::NoMoov)?;
    let mut file = Mp4File {
        major_brand: FourCc::default(),
        minor_version: 0,
        compatible_brands: Vec::new(),
        is_quicktime: true,
        timescale: 0,
        duration: 0,
        tracks: Vec::new(),
        metadata: Metadata::default(),
        fragmented: false,
        moov_offset,
        fragment_count: moofs.len(),
    };
    if let Some(f) = &ftyp {
        let mut c = Cur::labeled(f, "ftyp");
        file.major_brand = c.fourcc()?;
        file.minor_version = c.u32().unwrap_or(0);
        while c.remaining() >= 4 {
            file.compatible_brands.push(c.fourcc()?);
        }
        file.is_quicktime = file.major_brand == b"qt  ";
    }
    let mut trex: HashMap<u32, Trex> = HashMap::new();
    let mut trak_payloads = Vec::new();
    let mut meta_left = MAX_METADATA_BYTES;
    for b in boxes(&moov) {
        let b = b?;
        match &b.kind.0 {
            b"cmov" => return Err(Error::Unsupported("compressed movie header (cmov)".into())),
            b"mvhd" => {
                let mut c = Cur::labeled(b.payload, "mvhd");
                let (v, _) = c.full_header()?;
                if v == 1 {
                    c.skip(16)?;
                    file.timescale = c.u32()?;
                    file.duration = c.u64()?;
                } else {
                    c.skip(8)?;
                    file.timescale = c.u32()?;
                    file.duration = c.u32()? as u64;
                }
            }
            b"trak" => trak_payloads.push(b.payload),
            b"mvex" => {
                file.fragmented = true;
                for m in boxes(b.payload).map_while(|b| b.ok()) {
                    if m.kind == b"trex" {
                        let mut c = Cur::labeled(m.payload, "trex");
                        c.full_header()?;
                        let id = c.u32()?;
                        trex.insert(
                            id,
                            Trex { description_index: c.u32()?, duration: c.u32()?, size: c.u32()?, flags: c.u32()? },
                        );
                    }
                }
            }
            b"udta" => parse_udta(b.payload, &mut file.metadata, &mut meta_left),
            b"meta" => parse_meta(b.payload, &mut file.metadata, &mut meta_left),
            _ => {}
        }
    }
    let mut budget = SampleBudget::new(len);
    if let Some((_, max_samples)) = limits {
        if file.fragmented != init {
            return Err(Error::Unsupported(if init { "unfragmented init" } else { "fragmented MP4" }.into()));
        }
        budget.samples = budget.samples.min(max_samples);
    }
    for p in trak_payloads {
        let t = parse_trak(p, &file, len, &mut budget)?;
        file.tracks.push(t);
    }
    // Movie fragments.
    let mut next_dts: Vec<i64> = file
        .tracks
        .iter()
        .map(|t| t.samples.last().map(|s| s.dts.saturating_add(i64::from(s.duration))).unwrap_or(0))
        .collect();
    // A duplicate track ID resolves to its first track.
    let mut track_index = HashMap::new();
    for (i, t) in file.tracks.iter().enumerate() {
        track_index.entry(t.id).or_insert(i);
    }
    for (moof_off, moof) in &moofs {
        parse_moof(*moof_off, moof, &mut file.tracks, &track_index, &trex, &mut next_dts, &mut budget)?;
    }
    for t in &mut file.tracks {
        t.samples.compact();
        if file.fragmented && !moofs.is_empty() {
            let d: u64 = t.samples.iter().map(|s| s.duration as u64).sum();
            t.duration = t.duration.max(d);
            if t.timescale > 0 && file.timescale > 0 {
                let md = (t.duration as u128 * file.timescale as u128 / t.timescale as u128) as u64;
                file.duration = file.duration.max(md);
            }
        }
        // Timecode start frame from the first sample.
        if let Some(CodecConfig::Timecode(tc)) = t.entries.first_mut().map(|e| &mut e.codec)
            && let Some(s) = t.samples.first()
            && s.size >= 4
        {
            let mut b = [0u8; 4];
            if src.read_at(s.offset, &mut b).is_ok() {
                tc.start_frame = Some(u32::from_be_bytes(b));
            }
        }
    }
    Ok(file)
}

#[derive(Clone, Copy, Debug, Default)]
struct Trex {
    description_index: u32,
    duration: u32,
    size: u32,
    flags: u32,
}

fn parse_language(code: u16) -> String {
    if code < 0x400 {
        // QuickTime Macintosh language code.
        return if code == 0 { "eng".into() } else { "und".into() };
    }
    let c = |s: u16| (((code >> s) & 0x1F) as u8 + 0x60) as char;
    let s: String = [c(10), c(5), c(0)].into_iter().collect();
    if s.chars().all(|ch| ch.is_ascii_lowercase()) { s } else { "und".into() }
}

fn rescale(v: i128, from: u32, to: u32) -> i64 {
    if from == 0 {
        return 0;
    }
    (v * to as i128 / from as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn parse_trak(p: &[u8], file: &Mp4File, file_len: u64, budget: &mut SampleBudget) -> Result<Track> {
    let mut t = Track {
        id: 0,
        kind: TrackKind::Other,
        handler: FourCc::default(),
        handler_name: String::new(),
        timescale: 0,
        duration: 0,
        tkhd_duration: 0,
        language: "und".into(),
        width: 0,
        height: 0,
        enabled: true,
        alternate_group: 0,
        volume: 0,
        matrix: [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000],
        entries: Vec::new(),
        samples: SampleTable::default(),
        edits: Vec::new(),
        edit_offset: 0,
        references: Vec::new(),
        sdtp: Vec::new(),
        sample_groups: Vec::new(),
        pcm_chunked: false,
    };
    let mut mdia = None;
    for b in boxes(p) {
        let b = b?;
        match &b.kind.0 {
            b"tkhd" => {
                let mut c = Cur::labeled(b.payload, "tkhd");
                let (v, flags) = c.full_header()?;
                t.enabled = flags & 1 != 0;
                if v == 1 {
                    c.skip(16)?;
                    t.id = c.u32()?;
                    c.skip(4)?;
                    t.tkhd_duration = c.u64()?;
                } else {
                    c.skip(8)?;
                    t.id = c.u32()?;
                    c.skip(4)?;
                    t.tkhd_duration = c.u32()? as u64;
                }
                c.skip(8)?;
                let _layer = c.i16()?;
                t.alternate_group = c.u16()?;
                t.volume = c.i16()?;
                c.skip(2)?;
                for m in t.matrix.iter_mut() {
                    *m = c.i32()?;
                }
                t.width = c.u32()? >> 16;
                t.height = c.u32()? >> 16;
            }
            b"edts" => {
                if let Some(e) = find(b.payload, b"elst") {
                    let mut c = Cur::labeled(e, "elst");
                    let (v, _) = c.full_header()?;
                    let n = c.count(if v == 1 { 20 } else { 12 })?;
                    for _ in 0..n {
                        let (segment_duration, media_time) =
                            if v == 1 { (c.u64()?, c.i64()?) } else { (c.u32()? as u64, c.i32()? as i64) };
                        t.edits.push(Edit { segment_duration, media_time, media_rate: c.i32()? });
                    }
                }
            }
            b"tref" => {
                for r in boxes(b.payload).map_while(|b| b.ok()) {
                    let ids = r
                        .payload
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    t.references.push((r.kind, ids));
                }
            }
            b"mdia" => mdia = Some(b.payload),
            _ => {}
        }
    }
    let mdia = mdia.ok_or_else(|| Error::Invalid("trak without mdia".into()))?;
    let mut minf = None;
    for b in boxes(mdia) {
        let b = b?;
        match &b.kind.0 {
            b"mdhd" => {
                let mut c = Cur::labeled(b.payload, "mdhd");
                let (v, _) = c.full_header()?;
                if v == 1 {
                    c.skip(16)?;
                    t.timescale = c.u32()?;
                    t.duration = c.u64()?;
                } else {
                    c.skip(8)?;
                    t.timescale = c.u32()?;
                    t.duration = c.u32()? as u64;
                }
                t.language = parse_language(c.u16()?);
            }
            b"hdlr" => {
                let mut c = Cur::labeled(b.payload, "hdlr");
                c.full_header()?;
                let _pre = c.u32()?;
                t.handler = c.fourcc()?;
                c.skip(12)?;
                let name = c.rest();
                // ISO: null-terminated UTF-8; QuickTime: Pascal string.
                let s = if !name.is_empty() && name[0] as usize == name.len() - 1 && name.len() > 1 {
                    &name[1..]
                } else {
                    name.split(|&b| b == 0).next().unwrap_or(&[])
                };
                t.handler_name = String::from_utf8_lossy(s).into_owned();
            }
            b"minf" => minf = Some(b.payload),
            _ => {}
        }
    }
    t.kind = match &t.handler.0 {
        b"vide" => TrackKind::Video,
        b"soun" => TrackKind::Audio,
        b"tmcd" => TrackKind::Timecode,
        b"text" | b"sbtl" | b"subt" | b"clcp" => TrackKind::Subtitle,
        _ => TrackKind::Other,
    };
    // Edit offset in media timescale.
    let mut empty = 0i128;
    for e in &t.edits {
        if e.media_time == -1 {
            empty += e.segment_duration as i128;
        } else {
            t.edit_offset = rescale(empty, file.timescale, t.timescale).saturating_sub(e.media_time);
            break;
        }
    }
    if let Some(stbl) = minf.and_then(|m| find(m, b"stbl")) {
        parse_stbl(stbl, &mut t, file.is_quicktime, file_len, budget)?;
    }
    Ok(t)
}

struct SttsCursor<'a> {
    runs: &'a [(u32, u32)],
    run: usize,
    used: u32,
}

impl SttsCursor<'_> {
    /// Advance by `n` samples, returning the summed duration.
    fn take(&mut self, mut n: u64) -> u64 {
        let mut total = 0u64;
        while n > 0 && self.run < self.runs.len() {
            let (count, delta) = self.runs[self.run];
            let avail = (count - self.used) as u64;
            let k = avail.min(n);
            total = total.saturating_add(k.saturating_mul(delta as u64));
            n -= k;
            self.used += k as u32;
            if self.used >= count {
                self.run += 1;
                self.used = 0;
            }
        }
        total
    }
    fn next_value(&mut self) -> u32 {
        while self.run < self.runs.len() && self.runs[self.run].0 == 0 {
            self.run += 1;
        }
        let Some(&(_, d)) = self.runs.get(self.run) else { return 0 };
        self.take(1);
        d
    }
}

fn parse_stbl(stbl: &[u8], t: &mut Track, is_qt: bool, file_len: u64, budget: &mut SampleBudget) -> Result<()> {
    let mut stts: Vec<(u32, u32)> = Vec::new();
    let mut ctts: Vec<(u32, u32)> = Vec::new();
    let mut stss: Option<Vec<u32>> = None;
    let mut stsc: Vec<(u32, u32, u32)> = Vec::new();
    let mut const_size = 0u32;
    let mut sample_count = 0usize;
    let mut sizes: &[u8] = &[];
    let mut size_bits = 32usize;
    let mut chunks: Vec<u64> = Vec::new();
    let mut sbgp: Vec<(FourCc, Vec<(u32, u32)>)> = Vec::new();
    let mut sgpd: HashMap<FourCc, Vec<Vec<u8>>> = HashMap::new();
    for b in boxes(stbl) {
        let b = b?;
        let mut c = Cur::labeled(b.payload, "stbl child");
        match &b.kind.0 {
            b"stsd" => {
                c.full_header()?;
                let n = c.count(8)?;
                for e in boxes(c.rest()).take(n) {
                    let e = e?;
                    t.entries.push(parse_sample_entry(e.kind, e.payload, t.handler, is_qt)?);
                }
            }
            b"stts" => {
                c.full_header()?;
                let n = c.count(8)?;
                stts = (0..n).map(|_| Ok((c.u32()?, c.u32()?))).collect::<Result<_>>()?;
            }
            b"ctts" => {
                c.full_header()?;
                let n = c.count(8)?;
                ctts = (0..n).map(|_| Ok((c.u32()?, c.u32()?))).collect::<Result<_>>()?;
            }
            b"stss" => {
                c.full_header()?;
                let n = c.count(4)?;
                let mut v: Vec<u32> = (0..n).map(|_| c.u32()).collect::<Result<_>>()?;
                v.sort_unstable();
                stss = Some(v);
            }
            b"stsc" => {
                c.full_header()?;
                let n = c.count(12)?;
                stsc = (0..n).map(|_| Ok((c.u32()?, c.u32()?, c.u32()?))).collect::<Result<_>>()?;
            }
            b"stsz" => {
                c.full_header()?;
                const_size = c.u32()?;
                let n = c.u32()? as usize;
                if const_size == 0 {
                    c.check_count(n, 4)?;
                    sizes = c.rest();
                    size_bits = 32;
                    sample_count = n;
                } else {
                    // Samples of constant size must fit in the file: clamp absurd counts.
                    sample_count = (n as u64).min(file_len / const_size as u64) as usize;
                }
            }
            b"stz2" => {
                c.full_header()?;
                let field = c.u32()? & 0xFF;
                let n = c.u32()? as usize;
                let bits = match field {
                    4 | 8 | 16 => field as usize,
                    _ => return invalid(format!("stz2 field size {field}")),
                };
                if n.checked_mul(bits).is_none_or(|b| b.div_ceil(8) > c.remaining()) {
                    return Err(Error::Truncated("stz2"));
                }
                sizes = c.rest();
                size_bits = bits;
                const_size = 0;
                sample_count = n;
            }
            b"stco" => {
                c.full_header()?;
                let n = c.count(4)?;
                chunks = (0..n).map(|_| c.u32().map(|v| v as u64)).collect::<Result<_>>()?;
            }
            b"co64" => {
                c.full_header()?;
                let n = c.count(8)?;
                chunks = (0..n).map(|_| c.u64()).collect::<Result<_>>()?;
            }
            b"sdtp" => {
                c.full_header()?;
                t.sdtp = c.rest().to_vec();
            }
            b"sbgp" => {
                let (v, _) = c.full_header()?;
                let gt = c.fourcc()?;
                if v == 1 {
                    c.u32()?;
                }
                let n = c.count(8)?;
                let runs = (0..n).map(|_| Ok((c.u32()?, c.u32()?))).collect::<Result<_>>()?;
                sbgp.push((gt, runs));
            }
            b"sgpd" => {
                let (v, _) = c.full_header()?;
                let gt = c.fourcc()?;
                let default_len = if v >= 1 { c.u32()? } else { 0 };
                if v >= 2 {
                    c.u32()?;
                }
                let n = c.count(0)?;
                let mut descs = Vec::new();
                for _ in 0..n {
                    let l = if v >= 1 && default_len == 0 { c.u32()? } else { default_len };
                    if l == 0 {
                        // Version 0 without known length: keep the remainder as a single entry.
                        descs.push(c.rest().to_vec());
                        break;
                    }
                    descs.push(c.bytes(l as usize)?.to_vec());
                }
                sgpd.insert(gt, descs);
            }
            _ => {}
        }
    }
    t.sample_groups = sbgp
        .into_iter()
        .map(|(gt, runs)| SampleGroup { grouping_type: gt, runs, descriptions: sgpd.remove(&gt).unwrap_or_default() })
        .collect();
    if sample_count == 0 || chunks.is_empty() || stsc.is_empty() {
        return Ok(());
    }
    if stsc[0].0 == 0 || stsc.windows(2).any(|w| w[1].0 <= w[0].0) {
        return invalid("stsc first_chunk values are not increasing from 1");
    }
    let pcm = match t.entries.first().map(|e| &e.codec) {
        Some(CodecConfig::Pcm(p)) if const_size != 0 => Some(*p),
        _ => None,
    };
    let mut stts_c = SttsCursor { runs: &stts, run: 0, used: 0 };
    let mut ctts_c = SttsCursor { runs: &ctts, run: 0, used: 0 };
    let mut samples = SampleTable::default();
    let mut sidx = 0usize;
    let mut dts = 0i64;
    let mut stsc_i = 0usize;
    if let Some(p) = pcm {
        t.pcm_chunked = true;
        let a = t.audio().cloned().unwrap_or_default();
        // Frames come in units of `unit` frames and `unit_bytes` bytes (QuickTime v1 packets).
        let (unit, unit_bytes) = if a.qt_version == 1 && a.bytes_per_frame > 0 && a.samples_per_packet > 0 {
            (u64::from(a.samples_per_packet), u64::from(a.bytes_per_frame))
        } else if const_size > 1 {
            (1, u64::from(const_size))
        } else {
            (1, u64::from(p.bytes_per_frame()))
        };
        let frame_bytes = |frames: u64| -> u64 { (frames / unit).saturating_mul(unit_bytes) };
        let piece_frames = pcm_piece_frames(unit, unit_bytes);
        if chunks.len() > budget.samples {
            return Err(Error::TooLarge("chunk count"));
        }
        samples.reserve(chunks.len());
        for (ci, &off) in chunks.iter().enumerate() {
            while stsc_i + 1 < stsc.len() && stsc[stsc_i + 1].0 as usize <= ci + 1 {
                stsc_i += 1;
            }
            let spc = (stsc[stsc_i].1 as usize).min(sample_count - sidx);
            if spc == 0 {
                continue;
            }
            // A chunk past PCM_MAX_SAMPLE_BYTES becomes several samples.
            let mut left = spc as u64;
            let mut pos = off;
            while left > 0 {
                if samples.len() >= budget.samples {
                    return Err(Error::TooLarge("sample count"));
                }
                let split = frame_bytes(left) > PCM_MAX_SAMPLE_BYTES;
                let frames = if split { piece_frames } else { left };
                let dur = stts_c.take(frames);
                let size = frame_bytes(frames);
                if split {
                    // Each piece is at least half of PCM_MAX_SAMPLE_BYTES, so the file's size
                    // bounds how many a hostile chunk size can make.
                    budget.implied_bytes =
                        budget.implied_bytes.checked_sub(size).ok_or(Error::TooLarge("PCM chunk size"))?;
                }
                samples.push(Sample {
                    offset: pos,
                    size: size.min(u32::MAX as u64) as u32,
                    dts,
                    pts: dts,
                    duration: dur.min(u32::MAX as u64) as u32,
                    is_sync: true,
                    description_index: stsc[stsc_i].2,
                });
                dts = dts.saturating_add(dur.min(i64::MAX as u64) as i64);
                pos = pos.saturating_add(size);
                left -= frames;
            }
            sidx += spc;
            if sidx >= sample_count {
                break;
            }
        }
    } else {
        if const_size != 0 {
            sample_count = sample_count.min((budget.implied_bytes / const_size as u64) as usize);
        }
        if sample_count > budget.samples {
            return Err(Error::TooLarge("sample count"));
        }
        // Reserve only what the chunk map can place. A declared count far past it would keep
        // capacity the budget below never charges, track after track.
        let mut mapped = 0usize;
        let mut map_i = 0usize;
        for ci in 0..chunks.len() {
            while map_i + 1 < stsc.len() && stsc[map_i + 1].0 as usize <= ci + 1 {
                map_i += 1;
            }
            mapped = mapped.saturating_add(stsc.get(map_i).map_or(0, |e| e.1 as usize));
            if mapped >= sample_count {
                break;
            }
        }
        samples.try_reserve(sample_count.min(mapped)).map_err(|_| Error::TooLarge("sample count"))?;
        let mut stss_i = 0usize;
        'chunks: for (ci, &off) in chunks.iter().enumerate() {
            while stsc_i + 1 < stsc.len() && stsc[stsc_i + 1].0 as usize <= ci + 1 {
                stsc_i += 1;
            }
            let (_, spc, sdi) = stsc[stsc_i];
            let mut pos = off;
            for _ in 0..spc {
                if sidx >= sample_count {
                    break 'chunks;
                }
                let size = if const_size != 0 {
                    const_size
                } else {
                    match size_bits {
                        4 => {
                            ((sizes.get(sidx / 2).ok_or(Error::Truncated("stz2"))?
                                >> if sidx.is_multiple_of(2) { 4 } else { 0 })
                                & 0xF) as u32
                        }
                        8 => *sizes.get(sidx).ok_or(Error::Truncated("stz2"))? as u32,
                        16 => {
                            let b = sizes.get(sidx * 2..sidx * 2 + 2).ok_or(Error::Truncated("stz2"))?;
                            u16::from_be_bytes([b[0], b[1]]) as u32
                        }
                        _ => {
                            let b = sizes.get(sidx * 4..sidx * 4 + 4).ok_or(Error::Truncated("stsz"))?;
                            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
                        }
                    }
                };
                let duration = stts_c.next_value();
                let cts = ctts_c.next_value() as i32 as i64;
                let is_sync = match &stss {
                    None => true,
                    Some(v) => {
                        while stss_i < v.len() && (v[stss_i] as usize) < sidx + 1 {
                            stss_i += 1;
                        }
                        stss_i < v.len() && v[stss_i] as usize == sidx + 1
                    }
                };
                samples.push(Sample {
                    offset: pos,
                    size,
                    dts,
                    pts: dts.saturating_add(cts),
                    duration,
                    is_sync,
                    description_index: sdi,
                });
                pos = pos.saturating_add(size as u64);
                dts = dts.saturating_add(duration as i64);
                sidx += 1;
            }
        }
    }
    if pcm.is_none() && const_size != 0 {
        budget.implied_bytes -= samples.len() as u64 * const_size as u64;
    }
    budget.take(samples.len())?;
    t.samples = samples;
    Ok(())
}

fn parse_moof(
    moof_off: u64,
    moof: &[u8],
    tracks: &mut [Track],
    track_index: &HashMap<u32, usize>,
    trex: &HashMap<u32, Trex>,
    next_dts: &mut [i64],
    budget: &mut SampleBudget,
) -> Result<()> {
    // End of data of the previous traf, for the base-offset default of subsequent trafs.
    let mut prev_end: Option<u64> = None;
    for b in boxes(moof) {
        let b = b?;
        if b.kind != b"traf" {
            continue;
        }
        let mut tfhd = None;
        let mut tfdt = None;
        let mut truns = Vec::new();
        for c in boxes(b.payload) {
            let c = c?;
            match &c.kind.0 {
                b"tfhd" => tfhd = Some(c.payload),
                b"tfdt" => tfdt = Some(c.payload),
                b"trun" => truns.push(c.payload),
                _ => {}
            }
        }
        let tfhd = tfhd.ok_or_else(|| Error::Invalid("traf without tfhd".into()))?;
        let mut c = Cur::labeled(tfhd, "tfhd");
        let (_, fl) = c.full_header()?;
        let id = c.u32()?;
        let Some(&ti) = track_index.get(&id) else { continue };
        let d = trex.get(&id).copied().unwrap_or_default();
        let base = if fl & 1 != 0 {
            c.u64()?
        } else if fl & 0x20000 != 0 {
            moof_off
        } else {
            prev_end.unwrap_or(moof_off)
        };
        let sdi = if fl & 2 != 0 { c.u32()? } else { d.description_index };
        let def_dur = if fl & 8 != 0 { c.u32()? } else { d.duration };
        let def_size = if fl & 0x10 != 0 { c.u32()? } else { d.size };
        let def_flags = if fl & 0x20 != 0 { c.u32()? } else { d.flags };
        if let Some(p) = tfdt {
            let mut c = Cur::labeled(p, "tfdt");
            let (v, _) = c.full_header()?;
            let t = if v == 1 { c.u64()? } else { c.u32()? as u64 };
            next_dts[ti] = t.min(i64::MAX as u64) as i64;
        }
        let mut data_pos = base;
        for tr in truns {
            let mut c = Cur::labeled(tr, "trun");
            let (v, tf) = c.full_header()?;
            let per = [0x100u32, 0x200, 0x400, 0x800].iter().filter(|&&f| tf & f != 0).count() * 4;
            let n = c.u32()? as usize;
            if per > 0 {
                c.check_count(n, per)?;
            } else {
                let implied = (n as u64).saturating_mul(def_size.max(1) as u64);
                budget.implied_bytes =
                    budget.implied_bytes.checked_sub(implied).ok_or(Error::TooLarge("trun sample count"))?;
            }
            budget.take(n)?;
            if tf & 1 != 0 {
                data_pos = base.wrapping_add_signed(c.i32()? as i64);
            }
            let first_flags = if tf & 4 != 0 { Some(c.u32()?) } else { None };
            let t = &mut tracks[ti];
            t.samples.reserve(n);
            for i in 0..n {
                let dur = if tf & 0x100 != 0 { c.u32()? } else { def_dur };
                let size = if tf & 0x200 != 0 { c.u32()? } else { def_size };
                let flags = if tf & 0x400 != 0 { c.u32()? } else { def_flags };
                // first_sample_flags overrides the flags of the first sample only.
                let flags = if i == 0 { first_flags.unwrap_or(flags) } else { flags };
                let cto = if tf & 0x800 != 0 {
                    let raw = c.u32()?;
                    if v == 0 { raw as i64 } else { raw as i32 as i64 }
                } else {
                    0
                };
                let dts = next_dts[ti];
                t.samples.push(Sample {
                    offset: data_pos,
                    size,
                    dts,
                    pts: dts.saturating_add(cto),
                    duration: dur,
                    is_sync: flags & 0x1_0000 == 0,
                    description_index: sdi,
                });
                data_pos = data_pos.saturating_add(size as u64);
                next_dts[ti] = dts.saturating_add(dur as i64);
            }
        }
        prev_end = Some(data_pos);
    }
    Ok(())
}

/// Takes an entry of `key_len` and `value_len` bytes from `left`; false once it would not fit.
fn charge_metadata(left: &mut usize, key_len: usize, value_len: usize) -> bool {
    let cost = key_len.saturating_add(value_len).saturating_add(METADATA_ENTRY_COST);
    match left.checked_sub(cost) {
        Some(rest) => {
            *left = rest;
            true
        }
        None => false,
    }
}

/// Length of `String::from_utf8_lossy(bytes)` without building it: each invalid sequence
/// becomes one three-byte U+FFFD.
fn lossy_len(bytes: &[u8]) -> usize {
    bytes.utf8_chunks().map(|c| c.valid().len() + if c.invalid().is_empty() { 0 } else { 3 }).sum()
}

fn parse_udta(p: &[u8], md: &mut Metadata, left: &mut usize) {
    for b in boxes(p).map_while(|b| b.ok()) {
        if b.kind == b"meta" {
            parse_meta(b.payload, md, left);
        } else if b.kind.0[0] == 0xA9 && b.payload.len() >= 4 {
            let n = u16::from_be_bytes([b.payload[0], b.payload[1]]) as usize;
            // Charged at the stored text's length, before the value is converted.
            let key = b.kind.to_string_lossy();
            if let Some(s) = b.payload.get(4..4 + n)
                && charge_metadata(left, key.len(), lossy_len(s))
            {
                md.entries.push((key, String::from_utf8_lossy(s).into_owned()));
            }
        }
    }
}

fn parse_meta(p: &[u8], md: &mut Metadata, left: &mut usize) {
    // QuickTime `meta` is a plain box; ISO `meta` is a full box.
    let body = if p.len() >= 8 && &p[4..8] == b"hdlr" { p } else { p.get(4..).unwrap_or(&[]) };
    let mut keys: Vec<String> = Vec::new();
    if let Some(k) = find(body, b"keys") {
        let mut c = Cur::labeled(k, "keys");
        if c.full_header().is_ok()
            && let Ok(n) = c.count(8)
        {
            for _ in 0..n {
                let Ok(sz) = c.u32() else { break };
                let Ok(_ns) = c.u32() else { break };
                let Ok(name) = c.bytes((sz as usize).saturating_sub(8)) else { break };
                keys.push(String::from_utf8_lossy(name).into_owned());
            }
        }
    }
    let Some(ilst) = find(body, b"ilst") else { return };
    for item in boxes(ilst).map_while(|b| b.ok()) {
        let idx = item.kind.as_u32() as usize;
        let named = if idx >= 1 { keys.get(idx - 1) } else { None };
        if let Some(d) = find(item.payload, b"data")
            && d.len() >= 8
        {
            let ty = u32::from_be_bytes([d[0], d[1], d[2], d[3]]) & 0xFF_FFFF;
            // Charged before the key is copied (every item naming one long key is another copy)
            // and before the value is converted, at the lengths they will be stored at.
            if ty == 1 || ty == 0 {
                let fallback = if named.is_none() { Some(item.kind.to_string_lossy()) } else { None };
                let key_len = named.or(fallback.as_ref()).map_or(0, String::len);
                if charge_metadata(left, key_len, lossy_len(&d[8..])) {
                    let key = named.cloned().or(fallback).unwrap_or_default();
                    md.entries.push((key, String::from_utf8_lossy(&d[8..]).into_owned()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes() {
        // "und" = 0x55C4
        assert_eq!(parse_language(0x55C4), "und");
        assert_eq!(parse_language(0), "eng");
        assert_eq!(parse_language(0x15C7), "eng");
    }

    #[test]
    fn stts_cursor() {
        let runs = [(2, 10), (0, 99), (3, 5)];
        let mut c = SttsCursor { runs: &runs, run: 0, used: 0 };
        assert_eq!(c.next_value(), 10);
        assert_eq!(c.take(3), 10 + 5 + 5);
        assert_eq!(c.next_value(), 5);
        assert_eq!(c.next_value(), 0);
    }

    #[test]
    fn rejects_no_moov_and_cmov() {
        let mut b = crate::isobmff::bytes::BoxBuf::new();
        b.leaf(b"free", &[0; 4]);
        assert!(matches!(open(b.buf.clone()), Err(Error::NoMoov)));
        let m = b.start(b"moov");
        b.leaf(b"cmov", &[]);
        b.end(m);
        assert!(matches!(open(b.buf), Err(Error::Unsupported(_))));
    }

    #[test]
    fn metadata_qt_and_ilst() {
        let mut b = crate::isobmff::bytes::BoxBuf::new();
        let m = b.start(&[0xA9, b'n', b'a', b'm']);
        b.u16(2);
        b.u16(0);
        b.bytes(b"hi");
        b.end(m);
        let meta = b.start_full(b"meta", 0, 0);
        b.leaf(b"hdlr", &[0; 24]);
        let il = b.start(b"ilst");
        let it = b.start(&[0xA9, b'A', b'R', b'T']);
        let d = b.start(b"data");
        b.u32(1);
        b.u32(0);
        b.bytes(b"me");
        b.end(d);
        b.end(it);
        b.end(il);
        b.end(meta);
        let mut md = Metadata::default();
        parse_udta(&b.buf, &mut md, &mut { MAX_METADATA_BYTES });
        assert_eq!(md.title(), Some("hi"));
        assert_eq!(md.get("©ART"), Some("me"));
    }

    use crate::isobmff::bytes::BoxBuf;

    #[test]
    fn compact_samples_reconstruct_offsets_timing_and_flags() {
        let mut table = SampleTable::default();
        let mut reference = Vec::new();
        let (mut offset, mut dts) = (100u64, 0i64);
        for i in 0..1200 {
            // Cross bitmap words and checkpoints, backwards/large offsets, timestamp
            // discontinuities, description changes, and the unsigned trun cto range.
            if i % 73 == 0 {
                offset = 0;
            }
            if i % 97 == 0 {
                offset += u32::MAX as u64 + 100;
            }
            if i % 101 == 0 {
                dts += 300;
            }
            let duration = if i % 31 == 0 { 10 } else { 40 };
            let cto = if i == 900 { u32::MAX as i64 } else { (i % 3 - 1) * 80 };
            let s = Sample {
                offset,
                size: (i % 200) as u32,
                dts,
                pts: dts + cto,
                duration,
                is_sync: i % 17 == 0,
                description_index: (i / 100) as u32 + 1,
            };
            table.push(s);
            reference.push(s);
            offset += s.size as u64 + if i % 3 == 0 { 100 } else { 0 };
            dts += duration as i64;
        }
        table.compact();
        assert_eq!(table.iter().collect::<Vec<_>>(), reference);
        assert_eq!(table.iter().rev().collect::<Vec<_>>(), reference.iter().rev().copied().collect::<Vec<_>>());
        assert_eq!(table.get(table.len()), None);
        let mut track = open(handmade(None)).unwrap().tracks.remove(0);
        track.samples = table;
        let mut order: Vec<_> = reference.iter().enumerate().map(|(i, s)| (s.pts, i)).collect();
        order.sort_unstable();
        for target in [i64::MIN, -1, 0, 10, 10_000, 30_000, 50_000, i64::MAX] {
            let k = order.partition_point(|&(pts, _)| pts <= target).saturating_sub(1);
            assert_eq!(track.sample_at_pts(target), Some(order[k].1));
        }
    }

    fn full(b: &mut BoxBuf, kind: &[u8; 4], v: u8, f: impl FnOnce(&mut BoxBuf)) {
        let m = b.start_full(kind, v, 0);
        f(b);
        b.end(m);
    }

    /// Hand-built file: uuid/wide/skip boxes, stz2 sizes, co64, ctts v1 with negative offsets,
    /// stss, sdtp, sbgp/sgpd, and an edit list with a leading empty edit.
    fn handmade(stsz_override: Option<(u32, u32)>) -> Vec<u8> {
        let mut b = BoxBuf::new();
        let m = b.start(b"ftyp");
        b.bytes(b"isom");
        b.u32(0);
        b.bytes(b"isom");
        b.end(m);
        let m = b.start(b"uuid");
        b.bytes(&[0xAB; 16]);
        b.bytes(b"payload");
        b.end(m);
        b.leaf(b"wide", &[]);
        b.leaf(b"skip", &[1, 2, 3]);
        // mdat with 64-bit size: 4 samples of 3,1,2,4 bytes.
        let data_start = b.len() as u64 + 16;
        b.u32(1);
        b.bytes(b"mdat");
        b.u64(16 + 10);
        b.bytes(&[1, 1, 1, 2, 3, 3, 4, 4, 4, 4]);
        let moov = b.start(b"moov");
        full(&mut b, b"mvhd", 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(1000);
            b.u32(500);
            b.zeros(80);
        });
        let trak = b.start(b"trak");
        full(&mut b, b"tkhd", 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(7);
            b.u32(0);
            b.u32(500);
            b.zeros(52);
            b.u32(320 << 16);
            b.u32(240 << 16);
        });
        let edts = b.start(b"edts");
        full(&mut b, b"elst", 1, |b| {
            b.u32(2);
            b.u64(100);
            b.i64(-1);
            b.i32(0x10000);
            b.u64(400);
            b.i64(20);
            b.i32(0x10000);
        });
        b.end(edts);
        let mdia = b.start(b"mdia");
        full(&mut b, b"mdhd", 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(100);
            b.u32(40);
            b.u16(0x15C7);
            b.u16(0);
        });
        full(&mut b, b"hdlr", 0, |b| {
            b.u32(0);
            b.bytes(b"vide");
            b.zeros(12);
            b.bytes(b"Video\0");
        });
        let minf = b.start(b"minf");
        let stbl = b.start(b"stbl");
        full(&mut b, b"stsd", 0, |b| {
            b.u32(1);
            crate::isobmff::codec::SampleEntry::jpeg(320, 240).write(b, false).unwrap();
        });
        full(&mut b, b"stts", 0, |b| {
            b.u32(1);
            b.u32(4);
            b.u32(10);
        });
        full(&mut b, b"ctts", 1, |b| {
            b.u32(2);
            b.u32(2);
            b.i32(-5);
            b.u32(2);
            b.i32(5);
        });
        full(&mut b, b"stss", 0, |b| {
            b.u32(2);
            b.u32(3);
            b.u32(1);
        });
        full(&mut b, b"stsc", 0, |b| {
            b.u32(2);
            b.u32(1);
            b.u32(3);
            b.u32(1);
            b.u32(2);
            b.u32(1);
            b.u32(1);
        });
        match stsz_override {
            Some((size, count)) => full(&mut b, b"stsz", 0, |b| {
                b.u32(size);
                b.u32(count);
            }),
            None => full(&mut b, b"stz2", 0, |b| {
                b.u32(4);
                b.u32(4);
                b.u8(0x31);
                b.u8(0x24);
            }),
        }
        full(&mut b, b"co64", 0, |b| {
            b.u32(2);
            b.u64(data_start);
            b.u64(data_start + 6);
        });
        full(&mut b, b"sdtp", 0, |b| b.bytes(&[0x20, 0x10, 0x10, 0x10]));
        full(&mut b, b"sbgp", 0, |b| {
            b.bytes(b"roll");
            b.u32(1);
            b.u32(4);
            b.u32(1);
        });
        full(&mut b, b"sgpd", 1, |b| {
            b.bytes(b"roll");
            b.u32(2);
            b.u32(1);
            b.i16(-1);
        });
        b.end(stbl);
        b.end(minf);
        b.end(mdia);
        b.end(trak);
        b.end(moov);
        // Trailing box extending to EOF (size 0).
        b.u32(0);
        b.bytes(b"free");
        b.bytes(&[9; 5]);
        b.buf
    }

    #[test]
    fn handmade_sample_table() {
        let d = handmade(None);
        let f = open(d.as_slice()).unwrap();
        assert_eq!(f.major_brand, *b"isom");
        let t = &f.tracks[0];
        assert_eq!((t.id, t.kind, t.timescale, t.language.as_str()), (7, TrackKind::Video, 100, "eng"));
        assert_eq!((t.width, t.height), (320, 240));
        assert_eq!(t.handler_name, "Video");
        let sizes: Vec<u32> = t.samples.iter().map(|s| s.size).collect();
        assert_eq!(sizes, [3, 1, 2, 4]);
        let offs: Vec<u64> = t.samples.iter().map(|s| s.offset - t.samples.get(0).unwrap().offset).collect();
        assert_eq!(offs, [0, 3, 4, 6]);
        let pts: Vec<i64> = t.samples.iter().map(|s| s.pts).collect();
        assert_eq!(pts, [-5, 5, 25, 35]);
        let sync: Vec<bool> = t.samples.iter().map(|s| s.is_sync).collect();
        assert_eq!(sync, [true, false, true, false]);
        for (i, want) in [vec![1u8, 1, 1], vec![2], vec![3, 3], vec![4; 4]].iter().enumerate() {
            assert_eq!(&f.read_sample(d.as_slice(), 0, i).unwrap(), want);
        }
        // Empty edit of 100 movie ticks = 10 media ticks, then media starts at 20.
        assert_eq!(t.edits.len(), 2);
        assert_eq!(t.edit_offset, 10 - 20);
        assert_eq!(t.presentation_pts(2), Some(15));
        assert_eq!(t.sdtp.len(), 4);
        assert_eq!(t.sample_groups[0].grouping_type, *b"roll");
        assert_eq!(t.sample_groups[0].descriptions, vec![vec![0xFF, 0xFF]]);
        assert!(f.read_sample(d.as_slice(), 0, 4).is_err());
        assert!(f.read_sample(d.as_slice(), 1, 0).is_err());
    }

    #[test]
    fn constant_size_samples_share_one_file_budget() {
        // One chunk of u32::MAX constant-size samples makes a track build one sample per file
        // byte. Repeating the track must not repeat that allocation.
        let mut d = handmade(Some((1, u32::MAX)));
        let p = d.windows(4).position(|w| w == b"stsc").unwrap();
        d[p + 16..p + 20].copy_from_slice(&u32::MAX.to_be_bytes());
        let at = d.windows(4).position(|w| w == b"trak").unwrap() - 4;
        let size = u32::from_be_bytes(d[at..at + 4].try_into().unwrap()) as usize;
        let trak = d[at..at + size].to_vec();
        for _ in 0..3 {
            d.splice(at..at, trak.iter().copied());
        }
        let moov = d.windows(4).position(|w| w == b"moov").unwrap() - 4;
        let moov_size = u32::from_be_bytes(d[moov..moov + 4].try_into().unwrap()) + 3 * size as u32;
        d[moov..moov + 4].copy_from_slice(&moov_size.to_be_bytes());
        let f = open(d.as_slice()).unwrap();
        assert_eq!(f.tracks.len(), 4);
        assert!(f.tracks[0].samples.len() > d.len() / 2);
        assert!(f.tracks.iter().map(|t| t.samples.len()).sum::<usize>() <= d.len());
    }

    #[test]
    fn oversized_trailing_box_ends_the_scan() {
        let mut d = handmade(None);
        d.truncate(d.len() - 13);
        d.extend_from_slice(&1u32.to_be_bytes());
        d.extend_from_slice(b"free");
        d.extend_from_slice(&u64::MAX.to_be_bytes());
        d.extend_from_slice(&[0; 16]);
        assert_eq!(open(d.as_slice()).unwrap().tracks[0].samples.len(), 4);
    }

    #[test]
    fn one_long_key_named_by_many_items_stays_within_the_metadata_budget() {
        let key = vec![b'k'; 64 * 1024];
        let mut b = BoxBuf::new();
        b.leaf(b"hdlr", &[0; 24]);
        let keys = b.start_full(b"keys", 0, 0);
        b.u32(1);
        b.u32(8 + key.len() as u32);
        b.bytes(b"mdta");
        b.bytes(&key);
        b.end(keys);
        let il = b.start(b"ilst");
        for _ in 0..1000 {
            let it = b.start(&1u32.to_be_bytes());
            let d = b.start(b"data");
            b.u32(1);
            b.u32(0);
            b.bytes(b"v");
            b.end(d);
            b.end(it);
        }
        b.end(il);
        let mut payload = vec![0; 4];
        payload.extend_from_slice(&b.buf);
        let mut md = Metadata::default();
        parse_meta(&payload, &mut md, &mut { MAX_METADATA_BYTES });
        assert!(!md.entries.is_empty() && md.entries.len() < 1000, "{}", md.entries.len());
        assert!(md.entries.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() <= MAX_METADATA_BYTES);
        assert_eq!(md.entries[0].0.len(), key.len());
    }

    #[test]
    fn metadata_is_charged_at_its_stored_length() {
        assert_eq!(lossy_len(b"plain"), 5);
        for bytes in [&b"a\xFFb"[..], b"\xFF\xFE\xFD", b"\xE2\x82", "\u{a9}nam".as_bytes()] {
            assert_eq!(lossy_len(bytes), String::from_utf8_lossy(bytes).len(), "{bytes:?}");
        }
        // Invalid UTF-8 triples in size when converted; the budget sees the converted size.
        let mut b = BoxBuf::new();
        let m = b.start(&[0xA9, b'n', b'a', b'm']);
        b.u16(1000);
        b.u16(0);
        b.bytes(&[0xFF; 1000]);
        b.end(m);
        let mut md = Metadata::default();
        let mut left = 3000 + METADATA_ENTRY_COST + 5;
        parse_udta(&b.buf, &mut md, &mut left);
        assert_eq!(md.entries.len(), 1);
        assert_eq!(md.entries[0].0.len() + md.entries[0].1.len(), 3000 + 5 - left);
    }

    #[test]
    fn a_declared_sample_count_past_the_chunk_map_reserves_only_what_it_maps() {
        // stsz claims u32::MAX samples; the two chunks place four.
        let d = handmade(Some((1, u32::MAX)));
        let f = open(d.as_slice()).unwrap();
        assert_eq!(f.tracks[0].samples.len(), 4);
        assert!(f.tracks[0].samples.sizes.capacity() < 64, "{}", f.tracks[0].samples.sizes.capacity());
    }

    #[test]
    fn progressive_open_checks_header_and_sample_budgets() {
        let mut file = handmade(None);
        assert!(open_progressive(&file, 65536, 4).is_ok());
        assert!(matches!(open_progressive(&file, 1, 4), Err(Error::TooLarge(_))));
        assert!(matches!(open_progressive(&file, 65536, 3), Err(Error::TooLarge(_))));
        file.splice(..0, *b"\0\0\0\x08moof");
        assert!(matches!(open_progressive(&file, 65536, 4), Err(Error::Unsupported(_))));
    }

    #[test]
    fn absurd_counts_are_bounded() {
        // Constant-size stsz claiming 4 billion samples is clamped to what fits in the file.
        let d = handmade(Some((1, u32::MAX)));
        let f = open(d.as_slice()).unwrap();
        assert!(f.tracks[0].samples.len() <= d.len());
        // A table claiming more entries than its box holds fails cleanly.
        let mut d = handmade(None);
        let p = d.windows(4).position(|w| w == b"stts").unwrap();
        d[p + 8..p + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(open(d.as_slice()), Err(Error::Truncated(_))));
        // Box larger than its parent.
        let mut d = handmade(None);
        let p = d.windows(4).position(|w| w == b"stsc").unwrap();
        d[p - 4..p].copy_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
        assert!(open(d.as_slice()).is_err());
    }
    /// `data` followed by zeros up to `len` bytes.
    struct Padded(Vec<u8>, u64);

    impl ByteSource for Padded {
        fn len(&self) -> u64 {
            self.1
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(0);
            let start = (offset as usize).min(self.0.len());
            let end = (start + buf.len()).min(self.0.len());
            buf[..end - start].copy_from_slice(&self.0[start..end]);
            Ok(())
        }
    }

    #[test]
    fn samples_scale_with_the_file_length() {
        assert_eq!(SampleBudget::new(1000).samples, MIN_SAMPLE_ALLOWANCE);
        assert_eq!(SampleBudget::new(64 << 20).samples, 16 << 20);
        assert_eq!(SampleBudget::new(u64::MAX).samples, MAX_SAMPLES);
        // A constant-size stsz of one-byte samples claims as many as a 64 MiB file has bytes.
        let d = handmade(Some((1, u32::MAX)));
        assert!(matches!(open(Padded(d, 64 << 20)), Err(Error::TooLarge(_))));
    }

    #[test]
    fn a_pcm_chunk_past_4_gib_reads_as_bounded_samples() {
        use crate::isobmff::{Brand, Mp4Writer, PcmConfig, TrackConfig, WriteSample, WriterOptions};
        let pcm =
            PcmConfig { bits: 8, float: false, big_endian: false, signed: false, channels: 1, sample_rate: 48000.0 };
        let mut w = Mp4Writer::new(std::io::Cursor::new(Vec::new()), WriterOptions::new(Brand::Mov)).unwrap();
        let t = w.add_track(TrackConfig::new(SampleEntry::pcm(pcm), 48000)).unwrap();
        w.write_sample(t, WriteSample { data: &[0; 4], duration: 0, composition_offset: 0, is_sync: true }).unwrap();
        let mut d = w.finish_faststart().unwrap().into_inner();
        // One chunk of u32::MAX one-byte frames, as a writer without chunk limits makes for a
        // 4 GiB recording.
        let frames = u32::MAX.to_be_bytes();
        for (kind, at) in [(b"stts", 12), (b"stsz", 12), (b"stsc", 16)] {
            let p = d.windows(4).position(|w| w == kind).unwrap();
            d[p + at..p + at + 4].copy_from_slice(&frames);
        }
        // In a file too small for it, the claim is clamped to the file.
        let f = open(d.as_slice()).unwrap();
        assert!(f.tracks[0].samples.iter().map(|s| u64::from(s.size)).sum::<u64>() <= d.len() as u64);
        let len = d.len() as u64 + u64::from(u32::MAX);
        let f = open(Padded(d, len)).unwrap();
        let s = &f.tracks[0].samples;
        assert_eq!(s.len(), 256);
        assert!(s.iter().all(|s| u64::from(s.size) <= PCM_MAX_SAMPLE_BYTES));
        assert_eq!(s.iter().map(|s| u64::from(s.size)).sum::<u64>(), u64::from(u32::MAX));
        assert_eq!(s.iter().map(|s| u64::from(s.duration)).sum::<u64>(), u64::from(u32::MAX));
        assert!(
            s.iter()
                .zip(s.iter().skip(1))
                .all(|(a, b)| b.offset == a.offset + u64::from(a.size) && b.dts == a.dts + i64::from(a.duration))
        );
    }
}
