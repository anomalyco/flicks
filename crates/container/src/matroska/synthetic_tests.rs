//! Hand-built files exercising features FFmpeg does not write: unknown-size Segment/Clusters,
//! header stripping, BlockGroups, Colour + mastering metadata, Chapters, Tags, Attachments, CRC-32.

use crate::matroska::codec::Codec;
use crate::matroska::demux::MAX_TRACKS;
use crate::matroska::ebml::{self, write_id, write_unknown_size};
use crate::matroska::error::Error;
use crate::matroska::ids::*;
use crate::matroska::mux::{el, el_float, el_str, el_uint};
use crate::matroska::track::ContentEncoding;
use crate::matroska::{ByteSource, Demuxer, OpenOptions, Packet, open_with};

fn block(track: u8, rel: i16, flags: u8, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(flags);
    b.extend_from_slice(data);
    b
}

fn synthetic() -> Vec<u8> {
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el_uint(&mut h, DOC_TYPE_VERSION, 4);
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    // Info with a correct CRC-32 and a Void
    let mut info = Vec::new();
    el_uint(&mut info, TIMESTAMP_SCALE, 1_000_000);
    el_str(&mut info, TITLE, "Synthetic");
    el(&mut info, VOID, &[0; 3]);
    let mut crc_info = Vec::new();
    el(&mut crc_info, CRC32, &ebml::crc32(&info).to_le_bytes());
    crc_info.extend_from_slice(&info);
    el(&mut f, INFO, &crc_info);
    // Tracks
    let mut v = Vec::new();
    el_uint(&mut v, TRACK_NUMBER, 1);
    el_uint(&mut v, TRACK_UID, 11);
    el_uint(&mut v, TRACK_TYPE, 1);
    el_str(&mut v, CODEC_ID, "V_MPEG4/ISO/AVC");
    el(&mut v, CODEC_PRIVATE, &[1, 0x64, 0, 0x1F]);
    el_uint(&mut v, DEFAULT_DURATION, 40_000_000);
    let mut mm = Vec::new();
    for (id, val) in [
        (PRIMARY_R_X, 0.708),
        (PRIMARY_R_Y, 0.292),
        (PRIMARY_G_X, 0.17),
        (PRIMARY_G_Y, 0.797),
        (PRIMARY_B_X, 0.131),
        (PRIMARY_B_Y, 0.046),
    ] {
        el_float(&mut mm, id, val);
    }
    el_float(&mut mm, WHITE_POINT_X, 0.3127);
    el_float(&mut mm, WHITE_POINT_Y, 0.329);
    el_float(&mut mm, LUMINANCE_MAX, 1000.0);
    el_float(&mut mm, LUMINANCE_MIN, 0.0001);
    let mut col = Vec::new();
    el_uint(&mut col, MATRIX_COEFFICIENTS, 9);
    el_uint(&mut col, RANGE, 2);
    el_uint(&mut col, TRANSFER_CHARACTERISTICS, 16);
    el_uint(&mut col, PRIMARIES, 9);
    el_uint(&mut col, MAX_CLL, 1000);
    el_uint(&mut col, MAX_FALL, 400);
    el(&mut col, MASTERING_METADATA, &mm);
    let mut vid = Vec::new();
    el_uint(&mut vid, PIXEL_WIDTH, 1440);
    el_uint(&mut vid, PIXEL_HEIGHT, 1080);
    el_uint(&mut vid, DISPLAY_WIDTH, 1920);
    el_uint(&mut vid, DISPLAY_HEIGHT, 1080);
    el(&mut vid, COLOUR, &col);
    // rectangular, a quarter turn clockwise (portrait phone video)
    let mut proj = Vec::new();
    el_uint(&mut proj, PROJECTION_TYPE, 0);
    el_float(&mut proj, PROJECTION_POSE_ROLL, -90.0);
    el(&mut vid, PROJECTION, &proj);
    el(&mut v, VIDEO, &vid);
    // header stripping: every frame starts with 00 00 00 01
    let mut comp = Vec::new();
    el_uint(&mut comp, CONTENT_COMP_ALGO, 3);
    el(&mut comp, CONTENT_COMP_SETTINGS, &[0, 0, 0, 1]);
    let mut enc = Vec::new();
    el(&mut enc, CONTENT_COMPRESSION, &comp);
    let mut encs = Vec::new();
    el(&mut encs, CONTENT_ENCODING, &enc);
    el(&mut v, CONTENT_ENCODINGS, &encs);
    let mut a = Vec::new();
    el_uint(&mut a, TRACK_NUMBER, 2);
    el_uint(&mut a, TRACK_TYPE, 2);
    el_str(&mut a, CODEC_ID, "A_AAC/MPEG4/LC/SBR");
    el_str(&mut a, LANGUAGE, "ger");
    let mut au = Vec::new();
    el_float(&mut au, SAMPLING_FREQUENCY, 24000.0);
    el_float(&mut au, OUTPUT_SAMPLING_FREQUENCY, 48000.0);
    el_uint(&mut au, CHANNELS, 2);
    el(&mut a, AUDIO, &au);
    let mut tr = Vec::new();
    el(&mut tr, TRACK_ENTRY, &v);
    el(&mut tr, TRACK_ENTRY, &a);
    el(&mut f, TRACKS, &tr);
    // Chapters
    let mut disp = Vec::new();
    el_str(&mut disp, CHAP_STRING, "Intro");
    let mut atom = Vec::new();
    el_uint(&mut atom, CHAPTER_UID, 5);
    el_uint(&mut atom, CHAPTER_TIME_START, 0);
    el_uint(&mut atom, CHAPTER_TIME_END, 40_000_000);
    el(&mut atom, CHAPTER_DISPLAY, &disp);
    let mut ed = Vec::new();
    el(&mut ed, CHAPTER_ATOM, &atom);
    let mut ch = Vec::new();
    el(&mut ch, EDITION_ENTRY, &ed);
    el(&mut f, CHAPTERS, &ch);
    // Attachments
    let mut af = Vec::new();
    el_str(&mut af, FILE_NAME, "font.ttf");
    el_str(&mut af, FILE_MEDIA_TYPE, "font/ttf");
    el_uint(&mut af, FILE_UID, 77);
    el(&mut af, FILE_DATA, &[9; 100]);
    let mut att = Vec::new();
    el(&mut att, ATTACHED_FILE, &af);
    el(&mut f, ATTACHMENTS, &att);
    // Cluster 1 (unknown size): keyframe SimpleBlock, BlockGroup with ReferenceBlock + duration
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 1);
    el_uint(&mut f, TIMESTAMP, 1000);
    el(&mut f, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 1, 2]));
    el(&mut f, SIMPLE_BLOCK, &block(2, 5, 0x80, &[0xAA; 10]));
    let mut g = Vec::new();
    el(&mut g, BLOCK, &block(1, 40, 0, &[0x41, 3]));
    el(&mut g, REFERENCE_BLOCK, &[0xD8]);
    el_uint(&mut g, BLOCK_DURATION, 80);
    el(&mut f, BLOCK_GROUP, &g);
    el(&mut f, SIMPLE_BLOCK, &block(9, 0, 0x80, &[1])); // unknown track: ignored
    // Cluster 2 (unknown size), ended by Tags
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 8);
    el_uint(&mut f, TIMESTAMP, 1120);
    el(&mut f, SIMPLE_BLOCK, &block(1, 0, 0x80, &[0x65, 4]));
    let mut st = Vec::new();
    el_str(&mut st, TAG_NAME, "ARTIST");
    el_str(&mut st, TAG_STRING, "Nobody");
    let mut tg = Vec::new();
    let mut tgt = Vec::new();
    el_uint(&mut tgt, TAG_TRACK_UID, 11);
    el(&mut tg, TARGETS, &tgt);
    el(&mut tg, SIMPLE_TAG, &st);
    let mut tags = Vec::new();
    el(&mut tags, TAG, &tg);
    el(&mut f, TAGS, &tags);
    f
}

#[test]
fn synthetic_file() {
    let bytes = synthetic();
    let f = open_with(&bytes[..], &OpenOptions { index: true, verify_crc: true }).unwrap();
    assert!(f.live);
    assert!(f.warnings.is_empty(), "{:?}", f.warnings);
    assert_eq!((f.crc_checked, f.crc_errors.len()), (1, 0));
    assert_eq!(f.info.title.as_deref(), Some("Synthetic"));
    let v = &f.tracks[0];
    assert!(matches!(&v.codec, Codec::Avc { avcc } if avcc[1] == 0x64));
    let vi = v.video.as_ref().unwrap();
    assert_eq!(vi.pixel_aspect(), (4, 3));
    assert_eq!(vi.projection.as_ref().map(|p| p.roll), Some(-90.0));
    assert_eq!(vi.display_rotation(), Some(1));
    let c = vi.color.as_ref().unwrap();
    assert!(c.full_range());
    assert_eq!(
        (c.matrix_coefficients, c.transfer_characteristics, c.primaries, c.max_cll, c.max_fall),
        (Some(9), Some(16), Some(9), Some(1000), Some(400))
    );
    let m = c.mastering.as_ref().unwrap();
    assert_eq!(m.primaries[1], (0.17, 0.797));
    assert_eq!(m.white_point, (0.3127, 0.329));
    assert_eq!((m.luminance_max, m.luminance_min), (1000.0, 0.0001));
    assert_eq!(v.encodings, vec![ContentEncoding::HeaderStripping { bytes: vec![0, 0, 0, 1], scope: 1 }]);
    // legacy AAC ID: ASC synthesized with explicit SBR signaling (LC, 24 kHz, stereo → 48 kHz)
    assert_eq!(f.tracks[1].codec, Codec::Aac { asc: vec![0x13, 0x10, 0x56, 0xE5, 0x98] });
    assert_eq!(f.tracks[1].language, "ger");
    assert_eq!(f.chapters[0].chapters[0].title(), Some("Intro"));
    assert_eq!(f.chapters[0].chapters[0].end_ns, Some(40_000_000));
    assert_eq!(f.attachments.len(), 1);
    let at = &f.attachments[0];
    assert_eq!((at.name.as_str(), at.media_type.as_str(), at.uid), ("font.ttf", "font/ttf", 77));
    assert_eq!(&bytes[at.data_offset as usize..][..100], &[9; 100]);
    assert_eq!(f.tags[0].targets.track_uids, vec![11]);
    assert_eq!(f.tags[0].simple[0].string.as_deref(), Some("Nobody"));
    assert_eq!(f.clusters.len(), 2);
    assert_eq!(v.samples.len(), 3);
    assert_eq!(v.keyframes(), &[0, 2]);

    let d = Demuxer::from_slice(&bytes).unwrap();
    let p: Vec<Packet> = d.map(|p| p.unwrap()).collect();
    assert_eq!(p.len(), 4);
    assert_eq!((p[0].track, p[0].pts, p[0].keyframe, p[0].duration), (0, 1000, true, 40));
    assert_eq!(p[0].data, [0, 0, 0, 1, 0x65, 1, 2]);
    assert_eq!((p[1].track, p[1].pts, p[1].data.len()), (1, 1005, 10));
    assert_eq!((p[2].pts, p[2].keyframe, p[2].duration), (1040, false, 80));
    assert_eq!(p[2].data, [0, 0, 0, 1, 0x41, 3]);
    assert_eq!((p[3].pts, p[3].pts_ns), (1120, 1_120_000_000));
    assert_eq!(f.read_sample(&bytes[..], 0, 1).unwrap(), p[2].data);

    // CRC mismatch is reported, not fatal
    let mut bad = bytes.clone();
    let title = bad.windows(9).position(|w| w == b"Synthetic").unwrap();
    bad[title] = b's';
    let f = open_with(&bad[..], &OpenOptions { index: true, verify_crc: true }).unwrap();
    assert_eq!(f.crc_errors.len(), 1);

    // seeking without index or cues builds the index
    let mut d = Demuxer::with_options(&bytes[..], OpenOptions { index: false, verify_crc: false }).unwrap();
    assert_eq!(d.file().tracks.len(), 2);
    assert_eq!(d.seek(0, 1_100_000_000).unwrap().pts, 1000);
    assert_eq!(d.seek(0, 1_130_000_000).unwrap().pts, 1120);
    assert_eq!(d.next_packet().unwrap().unwrap().pts, 1120);
    assert!(d.file().indexed);
}

/// One video track and one cluster at timestamp `ts` holding `content`.
fn one_cluster(ts: u64, content: &[u8]) -> Vec<u8> {
    one_cluster_scaled(1_000_000, ts, content)
}

fn one_cluster_scaled(scale: u64, ts: u64, content: &[u8]) -> Vec<u8> {
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    let mut info = Vec::new();
    el_uint(&mut info, TIMESTAMP_SCALE, scale);
    el(&mut f, INFO, &info);
    let mut v = Vec::new();
    el_uint(&mut v, TRACK_NUMBER, 1);
    el_uint(&mut v, TRACK_UID, 1);
    el_uint(&mut v, TRACK_TYPE, 1);
    el_str(&mut v, CODEC_ID, "V_VP9");
    let mut tr = Vec::new();
    el(&mut tr, TRACK_ENTRY, &v);
    el(&mut f, TRACKS, &tr);
    write_id(&mut f, CLUSTER);
    write_unknown_size(&mut f, 8);
    el_uint(&mut f, TIMESTAMP, ts);
    f.extend_from_slice(content);
    f
}

fn one_block(ts: u64, rel: i16) -> Vec<u8> {
    let mut b = Vec::new();
    el(&mut b, SIMPLE_BLOCK, &block(1, rel, 0x80, &[0]));
    one_cluster(ts, &b)
}

/// A BlockGroup whose BlockAdditions hold `(BlockAddID, data)` pairs.
fn group_with_additions(block_data: &[u8], additions: &[(Option<u64>, &[u8])]) -> Vec<u8> {
    let mut adds = Vec::new();
    for (id, data) in additions {
        let mut more = Vec::new();
        if let Some(id) = id {
            el_uint(&mut more, BLOCK_ADD_ID, *id);
        }
        el(&mut more, BLOCK_ADDITIONAL, data);
        el(&mut adds, BLOCK_MORE, &more);
    }
    let mut g = Vec::new();
    el(&mut g, BLOCK, block_data);
    el(&mut g, BLOCK_ADDITIONS, &adds);
    let mut out = Vec::new();
    el(&mut out, BLOCK_GROUP, &g);
    out
}

#[test]
fn block_additional_id_1_is_located() {
    let mut c = group_with_additions(&block(1, 0, 0, b"frame"), &[(Some(2), b"other"), (None, b"alpha")]);
    c.extend(group_with_additions(&block(1, 40, 0, b"next"), &[(Some(2), b"other")]));
    // Xiph lacing with two frames: an addition belongs to the block, not to one frame.
    c.extend(group_with_additions(&block(1, 80, 0x02, &[1, 2, b'a', b'b', b'c']), &[(Some(1), b"laced")]));
    let bytes = one_cluster(0, &c);
    let f = open_with(&bytes[..], &OpenOptions { index: true, verify_crc: false }).unwrap();
    let t = &f.tracks[0];
    assert_eq!(t.samples.len(), 4);
    let adds: Vec<_> = (0..4).map(|i| f.read_sample_addition(&bytes[..], 0, i).unwrap()).collect();
    assert_eq!(adds, [Some(b"alpha".to_vec()), None, None, None]);
    assert_eq!(f.read_sample(&bytes[..], 0, 0).unwrap(), b"frame");
    assert!(f.read_sample_addition(&bytes[..], 0, 9).is_err());
}

#[test]
fn hostile_cluster_timestamps_saturate() {
    let pts = |bytes: &[u8]| {
        let first = Demuxer::from_slice(bytes).unwrap().next_packet().unwrap().unwrap().pts;
        let indexed =
            open_with(bytes, &OpenOptions { index: true, verify_crc: false }).unwrap().tracks[0].samples[0].pts;
        assert_eq!(first, indexed);
        first
    };
    assert_eq!(pts(&one_block(1000, 5)), 1005);
    assert_eq!(pts(&one_block(i64::MAX as u64, 0)), i64::MAX);
    assert_eq!(pts(&one_block(i64::MAX as u64, 1)), i64::MAX);
    assert_eq!(pts(&one_block(u64::MAX, -3)), i64::MAX - 3);
}

#[test]
fn cue_position_past_the_file_falls_back_to_the_index() {
    for pos in [1 << 40, u64::MAX - 1] {
        let mut ctp = Vec::new();
        el_uint(&mut ctp, CUE_TRACK, 1);
        el_uint(&mut ctp, CUE_CLUSTER_POSITION, pos);
        let mut cp = Vec::new();
        el_uint(&mut cp, CUE_TIME, 0);
        el(&mut cp, CUE_TRACK_POSITIONS, &ctp);
        let mut cues = Vec::new();
        el(&mut cues, CUE_POINT, &cp);
        let mut bytes = one_block(1000, 0);
        let at = bytes.windows(4).position(|w| w == CLUSTER.to_be_bytes()).unwrap();
        let mut el_cues = Vec::new();
        el(&mut el_cues, CUES, &cues);
        bytes.splice(at..at, el_cues);
        let mut d = Demuxer::with_options(&bytes[..], OpenOptions { index: false, verify_crc: false }).unwrap();
        assert_eq!(d.file().cues.len(), 1);
        assert_eq!(d.seek(0, 0).unwrap().pts, 1000);
    }
}

#[test]
fn huge_timestamp_scale_is_ignored() {
    // Three laced frames sharing the largest BlockDuration: with TimestampScale near 2^64 the
    // per-lace split overflowed u128 and i128.
    let mut g = Vec::new();
    el(&mut g, BLOCK, &block(1, 0, 0x02, &[2, 1, 1, b'a', b'b', b'c']));
    el_uint(&mut g, BLOCK_DURATION, u64::MAX);
    let mut c = Vec::new();
    el(&mut c, BLOCK_GROUP, &g);
    let f = open_with(
        &one_cluster_scaled(u64::MAX, i64::MAX as u64, &c)[..],
        &OpenOptions { index: true, verify_crc: false },
    )
    .unwrap();
    assert_eq!(f.info.timestamp_scale, 1_000_000);
    assert_eq!(f.tracks[0].samples.len(), 3);
    let f = open_with(&one_cluster_scaled(1 << 32, 0, &c)[..], &OpenOptions::default()).unwrap();
    assert_eq!(f.info.timestamp_scale, 1 << 32);
}

#[test]
fn duration_of_a_block_at_the_largest_timestamp_saturates() {
    for d in [80, u64::MAX] {
        let mut g = Vec::new();
        el(&mut g, BLOCK, &block(1, 0, 0, &[0]));
        el_uint(&mut g, BLOCK_DURATION, d);
        let mut c = Vec::new();
        el(&mut c, BLOCK_GROUP, &g);
        let f =
            open_with(&one_cluster(i64::MAX as u64, &c)[..], &OpenOptions { index: true, verify_crc: false }).unwrap();
        assert_eq!(f.duration_ns(), Some(u64::MAX));
    }
}

#[test]
fn empty_laced_frames_exhaust_the_frame_budget() {
    // Each 7-byte SimpleBlock is a fixed-size lace of 256 empty frames.
    let mut c = Vec::new();
    for _ in 0..100 {
        el(&mut c, SIMPLE_BLOCK, &block(1, 0, 0x04, &[0xFF]));
    }
    let r = open_with(&one_cluster(0, &c)[..], &OpenOptions { index: true, verify_crc: false });
    assert!(matches!(r, Err(Error::TooLarge(_))), "{r:?}");
    // Laces of one-byte frames are paid for by their bytes.
    let mut c = Vec::new();
    for _ in 0..4 {
        let mut data = vec![0xFF];
        data.extend_from_slice(&[0; 256]);
        el(&mut c, SIMPLE_BLOCK, &block(1, 0, 0x04, &data));
    }
    let f = open_with(&one_cluster(0, &c)[..], &OpenOptions { index: true, verify_crc: false }).unwrap();
    assert_eq!(f.tracks[0].samples.len(), 1024);
}

#[test]
fn track_entries_past_the_cap_are_rejected() {
    let file = |entries: usize| {
        let mut f = Vec::new();
        let mut h = Vec::new();
        el_str(&mut h, DOC_TYPE, "matroska");
        el(&mut f, EBML, &h);
        write_id(&mut f, SEGMENT);
        write_unknown_size(&mut f, 8);
        let mut tr = Vec::new();
        for _ in 0..entries {
            el(&mut tr, TRACK_ENTRY, &[0xD7, 0x81, 0x02]);
        }
        el(&mut f, TRACKS, &tr);
        f
    };
    assert_eq!(open_with(&file(3)[..], &OpenOptions::default()).unwrap().tracks.len(), 3);
    let r = open_with(&file(MAX_TRACKS + 1)[..], &OpenOptions::default());
    assert!(matches!(r, Err(Error::TooLarge(_))), "{r:?}");
}

/// Counts the bytes read from `data`.
struct Counting<'a>(&'a [u8], std::cell::Cell<u64>);

impl ByteSource for Counting<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.1.set(self.1.get() + buf.len() as u64);
        self.0.read_at(offset, buf)
    }
}

#[test]
fn overlapping_seek_head_targets_are_read_once() {
    // SeekHead entries point at nested Tags headers, each running to the end of the file.
    let (n, pad) = (64u64, 128usize << 10);
    let entry = |pos: u64| {
        let mut s = Vec::new();
        el(&mut s, SEEK_ID, &TAGS.to_be_bytes());
        el(&mut s, SEEK_POSITION, &pos.to_be_bytes());
        let mut o = Vec::new();
        el(&mut o, SEEK, &s);
        o
    };
    let seek_head = |first: u64| {
        let entries: Vec<u8> = (0..n).flat_map(|i| entry(first + i * 12)).collect();
        let mut o = Vec::new();
        el(&mut o, SEEK_HEAD, &entries);
        o
    };
    let mut f = Vec::new();
    let mut h = Vec::new();
    el_str(&mut h, DOC_TYPE, "matroska");
    el(&mut f, EBML, &h);
    write_id(&mut f, SEGMENT);
    write_unknown_size(&mut f, 8);
    let first = seek_head(0).len() as u64;
    f.extend_from_slice(&seek_head(first));
    let total = n * 12 + pad as u64;
    for i in 0..n {
        write_id(&mut f, TAGS);
        ebml::write_size(&mut f, total - (i + 1) * 12, 8);
    }
    f.resize(f.len() + pad, 0xEC);
    let src = Counting(&f, std::cell::Cell::new(0));
    let file = open_with(&src, &OpenOptions { index: false, verify_crc: false }).unwrap();
    assert_eq!(file.seek_head.len(), n as usize);
    assert!(src.1.get() <= 4 * f.len() as u64, "read {} bytes of a {}-byte file", src.1.get(), f.len());
}

/// Sparse, valid cluster layout: 12,500 four-MB frames, Cues at the end via SeekHead.
/// Payloads need not be allocated to measure the actual ByteSource reads made by open.
struct LargeMkv {
    head: Vec<u8>,
    cluster: Vec<u8>,
    cues: Vec<u8>,
    reads: std::cell::Cell<u64>,
}

#[test]
fn relative_cue_seeks_past_blocks_without_scanning_them() {
    for unknown_size in [false, true] {
        for (cue_time, shifted) in [(999, false), (1000, false), (1001, false), (1000, true)] {
            let mut body = Vec::new();
            for i in 0..48 {
                el(&mut body, SIMPLE_BLOCK, &block(1, i * 20, 0, &vec![0; 70_000]));
            }
            // one_cluster prefixes a three-byte Timestamp element.
            let relative = body.len() as u64 + 3;
            el(&mut body, SIMPLE_BLOCK, &block(1, 1000, 0x80, b"key"));
            let shifted_relative = body.len() as u64 + 3;
            el(&mut body, SIMPLE_BLOCK, &block(1, 2000, 0x80, b"wrong key"));
            let mut bytes = one_cluster(0, &body);
            let at = bytes.windows(4).position(|w| w == CLUSTER.to_be_bytes()).unwrap();
            let seg = bytes.windows(4).position(|w| w == SEGMENT.to_be_bytes()).unwrap() + 12;
            let cue = |position: u64, relative: u64| {
                let mut p = Vec::new();
                el_uint(&mut p, CUE_TRACK, 1);
                el(&mut p, CUE_CLUSTER_POSITION, &position.to_be_bytes());
                el(&mut p, CUE_RELATIVE_POSITION, &relative.to_be_bytes());
                let mut c = Vec::new();
                el_uint(&mut c, CUE_TIME, cue_time);
                el(&mut c, CUE_TRACK_POSITIONS, &p);
                let mut points = Vec::new();
                el(&mut points, CUE_POINT, &c);
                let mut out = Vec::new();
                el(&mut out, CUES, &points);
                out
            };
            let cue_len = cue(0, 0).len();
            if unknown_size {
                let (_, size_len) = ebml::read_size(&bytes[at + 4..]).unwrap();
                let mut unknown = Vec::new();
                write_unknown_size(&mut unknown, size_len);
                bytes[at + 4..at + 4 + size_len].copy_from_slice(&unknown);
            }
            let relative = if shifted { shifted_relative } else { relative };
            bytes.splice(at..at, cue((at + cue_len - seg) as u64, relative));
            let src = Counting(&bytes, std::cell::Cell::new(0));
            let mut d = Demuxer::new(&src).unwrap();
            let before = src.1.get();
            let key = d.keyframe_index(0).unwrap();
            assert_eq!(key.len(), 1);
            assert_eq!(key[0].relative_position, Some(relative));
            assert_eq!(src.1.get(), before);
            let point = d.seek(0, 1_000_000_000).unwrap();
            let packet = d.next_packet().unwrap().unwrap();
            assert_eq!((point.pts, packet.pts, packet.data.as_slice()), (1000, 1000, b"key".as_slice()));
            if !shifted {
                assert!(src.1.get() - before < 131_072, "seek read {} bytes", src.1.get() - before);
            }
            assert!(!d.file().indexed);
        }
    }
}

#[test]
fn cue_only_keyframes_do_not_scan_when_cues_are_missing() {
    let bytes = synthetic();
    let src = Counting(&bytes, std::cell::Cell::new(0));
    let mut d = Demuxer::new(&src).unwrap();
    let before = src.1.get();
    assert!(d.file().cue_keyframes(0).is_empty());
    assert!(d.file().cue_keyframes(usize::MAX).is_empty());
    assert_eq!(src.1.get(), before);
    assert!(!d.file().indexed);
    assert!(!d.keyframe_index(0).unwrap().is_empty());
    assert!(d.file().indexed);
}

#[test]
fn relative_cue_timestamp_probe_is_bounded() {
    // A missing timestamp behind many large Void elements must not cause a second
    // full traversal before the ordinary cluster-scan fallback.
    let mut body = Vec::new();
    for _ in 0..128 {
        el(&mut body, VOID, &vec![0; 70_000]);
    }
    let relative = body.len() as u64 + 3;
    el(&mut body, SIMPLE_BLOCK, &block(1, 1000, 0x80, b"key"));
    let mut base = one_cluster(0, &body);
    let at = base.windows(4).position(|w| w == CLUSTER.to_be_bytes()).unwrap();
    let seg = base.windows(4).position(|w| w == SEGMENT.to_be_bytes()).unwrap() + 12;
    let (_, size_len) = ebml::read_size(&base[at + 4..]).unwrap();
    // Replace one_cluster's leading three-byte Timestamp with a three-byte Void.
    base[at + 4 + size_len] = VOID as u8;
    let cue = |position: u64, relative: Option<u64>| {
        let mut p = Vec::new();
        el_uint(&mut p, CUE_TRACK, 1);
        el(&mut p, CUE_CLUSTER_POSITION, &position.to_be_bytes());
        if let Some(relative) = relative {
            el(&mut p, CUE_RELATIVE_POSITION, &relative.to_be_bytes());
        }
        let mut c = Vec::new();
        el_uint(&mut c, CUE_TIME, 1000);
        el(&mut c, CUE_TRACK_POSITIONS, &p);
        let mut points = Vec::new();
        el(&mut points, CUE_POINT, &c);
        let mut out = Vec::new();
        el(&mut out, CUES, &points);
        out
    };
    let mut scan_reads = 0;
    // None measures just the fallback. A far-away target tests the child budget;
    // a target at the first Void tests that the probe never passes that target.
    for relative in [None, Some(relative), Some(3)] {
        let mut bytes = base.clone();
        let cue_len = cue(0, relative).len();
        bytes.splice(at..at, cue((at + cue_len - seg) as u64, relative));
        let src = Counting(&bytes, std::cell::Cell::new(0));
        let mut d = Demuxer::new(&src).unwrap();
        let before = src.1.get();
        let point = d.seek(0, 1_000_000_000).unwrap();
        assert_eq!(point.pts, 1000);
        let reads = src.1.get() - before;
        match relative {
            None => scan_reads = reads,
            Some(3) => assert!(reads <= scan_reads + 65_536, "probe passed target: {reads} vs {scan_reads}"),
            Some(_) => assert!(reads <= scan_reads + 8 * 65_536, "unbounded probe: {reads} vs {scan_reads}"),
        }
        assert_eq!(d.next_packet().unwrap().unwrap().data, b"key");
        assert!(!d.file().indexed);
    }
}

impl LargeMkv {
    const CLUSTERS: u64 = 12_500;
    const STRIDE: u64 = 4_000_000;

    fn new() -> Self {
        let mut head = one_cluster(0, &[]);
        let start = head.windows(4).position(|w| w == CLUSTER.to_be_bytes()).unwrap();
        head.truncate(start);
        let seg = head.windows(4).position(|w| w == SEGMENT.to_be_bytes()).unwrap() + 12;
        let seek = |position: u64| {
            let mut entry = Vec::new();
            el(&mut entry, SEEK_ID, &CUES.to_be_bytes());
            el(&mut entry, SEEK_POSITION, &position.to_be_bytes());
            let mut entries = Vec::new();
            el(&mut entries, SEEK, &entry);
            let mut out = Vec::new();
            el(&mut out, SEEK_HEAD, &entries);
            out
        };
        let first = (head.len() + seek(0).len() - seg) as u64;
        head.extend(seek(first + Self::CLUSTERS * Self::STRIDE));
        let mut cluster = Vec::new();
        write_id(&mut cluster, CLUSTER);
        ebml::write_size(&mut cluster, Self::STRIDE - 12, 8);
        el(&mut cluster, TIMESTAMP, &0u64.to_be_bytes());
        write_id(&mut cluster, SIMPLE_BLOCK);
        let size = Self::STRIDE - cluster.len() as u64 - 8;
        ebml::write_size(&mut cluster, size, 8);
        cluster.extend(block(1, 0, 0x80, &[]));
        let mut points = Vec::new();
        for i in 0..Self::CLUSTERS {
            let mut position = Vec::new();
            el_uint(&mut position, CUE_TRACK, 1);
            el_uint(&mut position, CUE_CLUSTER_POSITION, first + i * Self::STRIDE);
            el_uint(&mut position, CUE_RELATIVE_POSITION, 10);
            let mut point = Vec::new();
            el_uint(&mut point, CUE_TIME, i * 1000);
            el(&mut point, CUE_TRACK_POSITIONS, &position);
            el(&mut points, CUE_POINT, &point);
        }
        let mut cues = Vec::new();
        el(&mut cues, CUES, &points);
        Self { head, cluster, cues, reads: std::cell::Cell::new(0) }
    }
}

impl ByteSource for LargeMkv {
    fn len(&self) -> u64 {
        self.head.len() as u64 + Self::CLUSTERS * Self::STRIDE + self.cues.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.reads.set(self.reads.get() + buf.len() as u64);
        buf.fill(0);
        let tail = self.head.len() as u64 + Self::CLUSTERS * Self::STRIDE;
        let copy = |start: u64, bytes: &[u8], buf: &mut [u8]| {
            let lo = offset.max(start);
            let hi = (offset + buf.len() as u64).min(start + bytes.len() as u64);
            if lo < hi {
                buf[(lo - offset) as usize..(hi - offset) as usize]
                    .copy_from_slice(&bytes[(lo - start) as usize..(hi - start) as usize]);
            }
        };
        copy(0, &self.head, buf);
        if offset < tail {
            let i = offset.saturating_sub(self.head.len() as u64) / Self::STRIDE;
            for j in i..=(i + 1).min(Self::CLUSTERS - 1) {
                copy(self.head.len() as u64 + j * Self::STRIDE, &self.cluster, buf);
                copy(self.head.len() as u64 + j * Self::STRIDE + 14, &(j * 1000).to_be_bytes(), buf);
            }
        }
        copy(tail, &self.cues, buf);
        Ok(())
    }
}

#[test]
fn large_mkv_open_measurement() {
    let src = LargeMkv::new();
    let start = std::time::Instant::now();
    let mut d = Demuxer::new(&src).unwrap();
    let open_time = start.elapsed();
    let open_bytes = src.reads.get();
    let start = std::time::Instant::now();
    let keys = d.keyframe_index(0).unwrap();
    eprintln!(
        "50 GB MKV: open={open_time:?}, bytes={open_bytes}, cues={} index={:?}, total_bytes={}",
        keys.len(),
        start.elapsed(),
        src.reads.get()
    );
    assert_eq!(keys.len(), LargeMkv::CLUSTERS as usize);
    assert!(open_bytes < 1_000_000);
    assert_eq!(src.reads.get(), open_bytes, "Cues index must not read clusters");
    assert!(!d.file().indexed);
    assert!(d.tracks()[0].samples.is_empty());
}
