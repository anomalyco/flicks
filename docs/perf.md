# Performance measurements

## FLK-29 — container open (2026-10-30)

Apple M5 Max, macOS 27.2, rustc 1.99.0, release profile. Bytes are decimal.
Baseline: `83cb63d`. These are individual local measurements, not disk-throughput
or cross-machine guarantees.

| Measurement | Before | After |
|---|---:|---:|
| 50 GB MKV: bytes read by default open | 819,539,388 | **404,924** |
| Same MKV: open time | 8.187 ms | **0.627 ms** |
| 12,500-key MKV index | 0.089 ms, after the full open scan | **0.100 ms**, from Cues, no additional reads |
| 2 h H.264 B-frame + AAC MP4: retained heap added by open | 24,991,088 bytes | **4,790,128 bytes** |
| Same MP4: open time | 10.321 ms | **8.536 ms** |
| Same MP4: PTS lookup at 1 h, video / audio | not recorded | **6.667 / 7.542 µs** |

### Method and scope

- MKV: the existing synthetic test module supplies a sparse `ByteSource` with
  12,500 four-MB clusters and end-of-file Cues reached through SeekHead. Cluster
  timestamps and Cues agree; frame payloads are zero-filled on demand, not
  allocated or decoded. The counter counts all requested bytes, including cache
  read-ahead. Thus the byte result exercises the actual demuxer, while its timing
  measures parsing and memory copies, not physical disk I/O.
- The relative-Cue test seeks past 48 preceding 70 KB blocks, for both known- and
  unknown-size Clusters. It verifies the returned keyframe and packet, no full
  index, and fewer than 131,072 additional bytes read. Missing/invalid relative
  positions retain the cluster-scan fallback; missing Cues retain full indexing.
- MP4: the existing two-second H.264 B-frame/AAC oracle fixture is stream-looped
  and remuxed by external FFmpeg to two hours. It contains 178,102 video samples
  and 338,391 audio samples, including loop-boundary timestamp variation.
  The demuxer opens a file source, not an in-memory copy of the media. macOS
  `/usr/bin/heap` snapshots immediately before and after `open` measure incremental
  live allocations, including allocator rounding. The same measurement was run
  against an isolated source snapshot of the baseline under ignored `target/`.
- **The MP4 memory figure is retained heap after open returns, not peak RSS or
  peak parsing memory.** Temporary `moov` and parsed table buffers are released
  before the second snapshot; transient parsing allocations can exceed 5 MB.
  The result is for this representative two-track fixture, not a bound for
  arbitrary frame rates, track counts, fragments, or hostile metadata.

### Reproduce

```sh
CARGO_PROFILE_RELEASE_STRIP=none cargo test -p flicks-container --release --lib large_mkv_open_measurement -- --nocapture
CARGO_PROFILE_RELEASE_STRIP=none cargo test -p flicks-container --release --test oracle_demux two_hour_mp4_open_memory -- --ignored --nocapture --test-threads=1
CRAFT_MEDIA_REQUIRE_ORACLES=1 FLICKS_REQUIRE_ORACLES=1 cargo test -p flicks-container --test fuzz --test fuzz_matroska --test oracle_demux --test oracle_matroska --test oracle_mux
scripts/check.sh
```

`STRIP=none` avoids this machine's missing `libLLVM.dylib` in `rust-objcopy`;
optimization and LTO remain enabled. The heap benchmark is macOS-only and ignored
in the ordinary suite; it generates its media under `target/fixtures`, never in
git. Run it alone for meaningful heap deltas.

Verification: the workspace check passed (format, Clippy `-D warnings`, dependency
policy, tests and doctests). Container coverage: 95 unit tests, seven roundtrip
tests, 49 oracle tests and five mutation/truncation fuzz tests passed. The fuzz
suites exercise 90,000 MP4 and 8,000 MKV mutations plus prefix truncations. Existing
fixture-generation ignores and the known Vorbis laced-remux ignore are unchanged.

### API changes

- Matroska defaults to metadata/Cues only. Request `OpenOptions { index: true,
  ..Default::default() }` or call `build_index` for per-frame sample tables and
  derived duration when Info has no duration. Cue-derived `Keyframe` entries have
  optional sample/absolute-block fields, an absolute cluster offset and the
  optional Cluster-payload-relative offset; they do not invent sample numbers.
- MP4 `Track::samples` is a compact `SampleTable`: use `get(i)` or `iter()` for
  `Sample` values, not vector indexing or borrowed samples. `sync_samples()` now
  yields an iterator. Sparse offset checkpoints, time runs, and 256-sample PTS
  bounds replace expanded samples and the full PTS/sync permutations. PCM,
  fragmented files, arbitrary sample-description changes and wide fragment
  composition offsets continue to use the same sample-reading API.

## FLK-8 — library scan (2026-10-30)

Apple M5 Max, macOS 27.2, rustc 1.99.0, release profile, warm page cache, APFS temp
directory. Catalog in an in-memory SQLite database, no TMDB key, one thread.

| Measurement | Result |
|---|---:|
| First scan, 10,000 files (241 folders), every file opened, probed and hashed | **0.77 / 0.85 / 0.84 s** (3 runs) |
| Unchanged rescan of the same library: folders listed, media files opened | **24 / 25 / 29 ms**, 241 listings, **0 opens** |
| New file visible through the watcher (FSEvents, write to catalog row) | **111–122 ms** (5 runs) |

### Method and scope

- Synthetic library: 40 shows × 5 seasons × 50 episodes. Each file is a copy of one real
  11,547-byte, 60-second Matroska file (H.264 track, 6-channel AAC, SRT; 60 Cues) made by
  `container`'s own muxer, then extended with `set_len` to 4 GB plus a per-file offset, so
  every file has a different size and OSHash. The extension is a sparse hole: the OSHash
  reads the last 64 KiB of every file, which costs nothing on a hole. **The scan therefore
  measures the open, header parse, hash reads, name parsing and SQLite writes, not reads of
  large files from a cold disk.** A library on a spinning or network disk will be slower,
  and the page cache was warm.
- Each directory is one transaction. Probes are sequential; nothing is tuned.
- "Opens" is `ScanReport::opened`, counted at the one `File::open` that probes a file; the
  test `unchanged_rescan_opens_no_media_files` asserts it is 0 on an unchanged tree and 1
  after one file is rewritten.
- Watcher figure: the time from writing a new MKV into a watched folder to the scan of
  the reported folder finding it in the catalog, including the watcher's 100 ms quiet period.
  Events that FSEvents replays from before it started are dropped first.

### Reproduce

```sh
cargo test -p flicks-library --release -- --ignored --nocapture first_scan_of_10k_files
cargo test -p flicks-library -- --nocapture a_new_file_appears_within_two_seconds
```
