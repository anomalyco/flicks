# Performance measurements

## FLK-11 — signed range first byte and playback metadata (2026-10-30)

Apple M5 Max, macOS 27.2, rustc 1.99.0, release profile, code `dc65301`.
Before: the API hooks returned 404; there was no range-serving baseline.

| File | Size (bytes) | Keyframes | Range first byte p50 / p95 / max (ms) | Complete `/play` p50 / p95 / max (ms) |
|---|---:|---:|---:|---:|
| Generated 5-second H.264/AAC MP4 | 501,176 | 5 | 0.528 / **0.633** / 1.039 | 0.534 / 0.639 / 0.891 |
| Real 1080p H.264/AAC episode A | 1,203,733,864 | 1,382 | 0.585 / **0.710** / 0.776 | 0.945 / 1.107 / 1.259 |
| Real 1080p H.264/AAC episode B | 1,455,959,793 | 1,657 | 0.571 / **0.657** / 0.710 | 0.994 / 1.092 / 1.853 |

Each run launches the actual `flicks serve` binary with a temporary HOME/database, adds its
folder with `flicks folder add`, and pairs a viewer. Each row is 100 requests after one warmup,
on `127.0.0.1`, with **new TCP connections and pinned TLS handshakes for every request**.
Range is `bytes=0-65535`; the timer starts after client signing, before TCP connect, and ends
when Hyper yields the first nonempty media body frame (not just response headers). It includes
server signature verification, DB lookup, containment/symlink checks, file open/read and TLS.
`/play` includes client signing through the complete JSON body and parsing, with facts, tracks,
the full keyframe index, progress and startup ranges. It is one metadata request, not one
request for all media bytes. The range body is drained after timing; file frames are bounded
to 64 KiB. File/OS caches are warm; these are localhost results, not a physical-LAN or cold-disk
claim. All 300 range measurements were under 5 ms.

Folder-add through catalog visibility was 31.113 ms for the generated file and 28.384 ms for
both real episodes (includes CLI process, scan and 20 ms readiness-poll granularity). Real media
was read-only from `/Users/vogel/dev/flicks-test-media/TV`; nothing was copied into the repository
or changed. Temporary servers exit and temporary HOME directories are removed after each run.

Reproduce (port 7686 must be free):

```sh
cargo test -p flicks-server --release --test server api_performance -- --ignored --nocapture --test-threads=1
FLICKS_API_MEDIA=/Users/vogel/dev/flicks-test-media/TV cargo test -p flicks-server --release --test server api_performance -- --ignored --nocapture --test-threads=1
```

`scripts/check.sh` passes: 720 tests passed, 0 failed, 25 ignored across 55 test/doc groups.
New coverage includes startup/add/remove/watch scanning, signed viewer catalog access, one-request
MP4/MKV playback metadata, tail-moov header ranges, newest-wins progress and user isolation,
200/206/416 cases (empty files, closed/open-ended/suffix ranges, overflow, out-of-bounds and
multiple ranges), signature/Range binding, range-parser mutations, removed folders, traversal
and file/directory symlinks repointed after scanning. The timing test is opt-in and ran separately.

### Review fixes remeasurement (2026-10-30)

Code `e66232b`. After replacing list-route N+1 lookups with joined rows, restricting the symlink walk to the
library root, and making playback headers match the scanner's tolerant parsing:

| File | Range first byte p50 / p95 / max (ms) | Complete `/play` p50 / p95 / max (ms) |
|---|---:|---:|
| Generated MP4 | 0.501 / **0.599** / 0.708 | 0.508 / 0.660 / 0.724 |
| Real episode A | 0.525 / **0.687** / 0.867 | 0.873 / 1.118 / 1.365 |
| Real episode B | 0.583 / **0.742** / 0.784 | 1.014 / 1.173 / 1.279 |

Same fixtures, machine, method and 100 measured requests per file as above; all 300 range
requests remain below 5 ms. Range p95 was 0.633 / 0.710 / 0.657 ms before, and is now
0.599 / 0.687 / 0.742 ms; this small mixed movement is not a claimed speedup.
Folder-add to visibility: 30.522 ms generated, 30.508 ms for both real files. The existing
measurement exercises `/items` for readiness, but does not isolate large-list latency.
Run the same reproduction commands with `CARGO_PROFILE_RELEASE_STRIP=none` to avoid the
machine's `rust-objcopy`/`libLLVM.dylib` issue.

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

Review-fix remeasurement (same machine and fixture, one release run): replacing the MKV
Demuxer with `matroska::open` and `cue_keyframes` gives **704 ms** for the first 10,000-file
scan and **22.7 ms** for the unchanged rescan (241 listings, zero media opens). The earlier
runs were 770–850 ms and 24–29 ms respectively; warm-cache run-to-run variation applies.
Command: `CARGO_PROFILE_RELEASE_STRIP=none cargo test -p flicks-library --release -- --ignored --nocapture first_scan_of_10k_files`.

## FLK-22 — subtitle parse and active-cue lookup (2026-10-30)

Apple M5 Max, macOS 27.2, rustc 1.99.0, release profile, code `dec9210`. One thread; the player
asks "which cues are active" once per displayed frame, so lookup is the hot path.

| File (2 hours, 2 lines per cue) | Parse | `active_at`, 171,428 lookups (every 42 ms) |
|---|---:|---:|
| 1,500 cues (141 KiB) | 1.2 ms | **43 ns** avg, worst 17–63 µs |
| 2,500 cues (237 KiB) | 1.8–2.3 ms | **45 ns** avg, worst 21–47 µs |
| 10,000 cues (954 KiB) | 6.0–6.9 ms | **49 ns** avg, worst 23–44 µs |
| 99,999 short cues plus one cue lasting the whole film (worst case for the lookup) | not measured (above the 8 MiB input cap as SRT) | **24 µs** avg, worst 147–179 µs |

Lookup is a binary search on start time plus a walk back while some earlier cue may still be
showing (a running maximum of end times). Normal files walk back 0–2 cues. The last row is the
adversarial case: a cue that never ends keeps the walk going to the first cue, so cost grows with
the cue count, here 100,000 (the cap). That is 24 µs against a 16,667 µs frame at 60 fps, so it is
left as is. "Worst" is the slowest single call, which includes timer noise and one-off cache misses.

Reproduce: `cargo test -p flicks-subtitle --release -- --ignored --nocapture lookup_cost`.
The file is generated in the test (made-up text); three runs for the first three rows, two for the last.

## FLK-4 — direct video draw (2026-10-30)

Apple M5 Max (not M1), macOS, wgpu 30.0.1 Metal; debug Rust test harness, normal GPU
shader compilation. 3840×2160 P010 Y + UV textures, BT.2020 primaries, limited range,
1000-nit HDR source peak; draw to 3840×2160 BGRA8, sRGB-encoded output. No intermediate
RGBA texture. Each row is 64 separately submitted draws after 8 warmups.

| Transfer | Latest median | p95 | Maximum |
|---|---:|---:|---:|
| SDR BT.1886 | 0.7819 ms | 2.6208 ms | 3.4885 ms |
| PQ → SDR, BT.2390 | 1.2615 ms | 3.2772 ms | 3.9480 ms |
| HLG → SDR, BT.2390 | 1.1189 ms | 2.5548 ms | 3.4081 ms |

An earlier serial run measured medians **0.3330 / 0.4535 / 0.7783 ms** and p95
**1.0569 / 1.2160 / 2.0635 ms**, respectively (before the fixture also exercised P010's
lowest two significant bits). Thus **the <1 ms criterion is not reproducibly met**;
the faster run is not a completion claim. This is a shared desktop, not an isolated GPU
benchmark environment. Contention/stage-counter latency is a hypothesis, not a proven cause.
Stopped measurement retries for coordinator review; no speculative LUT/compute optimization.

Method: `RenderPassTimestampWrites` samples start-of-vertex and end-of-fragment, including
the pass rather than just shader ALU. Upload and `prepare` happen before the timed pass;
CPU preparation, upload, readback and presentation are **not** included. Resolve queries in
a separate submission after device completion: resolving in the producing command buffer
on this driver returned stale/zero end counters. The harness rejects reversed/zero pairs.
Passes are individually submitted and completed; batching them let vertex stages run ahead
of fragment stages and did not measure isolated draw latency.

```sh
cargo test -p flicks-gpu p010_4k_gpu_time -- --ignored --nocapture
```

The benchmark asserts median <1 ms for all three transfers and **fails on the latest run**.
The normal `scripts/check.sh` gate passes (benchmark intentionally ignored). Correctness:
336 in-memory golden images generated by an independent f64 reference, all within 1/255
(allowed 2/255), covering NV12/P010, all three matrices, both ranges, SDR/sRGB/PQ/HLG,
RGBA/BGRA and linear/encoded Rgba16Float layer targets. 24 IOSurface/CPU comparisons are
byte-identical, and 12 cases retain correct pixels after dropping the source buffer,
imported frame and texture cache before submitting the draw.

## FLK-30 — audio resampler (2026-10-30)

Apple M5 Max, macOS 27.2, rustc 1.99.0, release profile, code `073ef32`. Single thread,
10 s of a stereo 1 kHz sine pushed in 1024-frame chunks; best of three runs.

| Conversion | CPU per second of stereo audio | Share of one core |
|---|---:|---:|
| 44.1 → 48 kHz | **2.78 ms** (2,777 µs) | 0.28 % |
| 48 → 44.1 kHz | **2.73 ms** (2,726 µs) | 0.27 % |

Rational polyphase, Kaiser-windowed sinc (64 zero crossings of the lower rate, 64–70 taps per
output frame). Quality against computed sines: THD+N −100.5 to −109.2 dB for 1, 10 and 18 kHz
both ways, max error ≤ 1.7·10⁻⁵, a 23 kHz tone aliases at −94.6 dB going to 44.1 kHz. Not
optimized: nothing calls for it at this cost.

```sh
cargo test -p flicks-audio --release --test resample -- --nocapture --test-threads=1
```

## FLK-3 — VideoToolbox decode (2026-10-30)

Apple M5 Max, macOS 27.2 (26B5112a), Rust 1.99.0, release profile with LTO.
This is the available Mac, **not an M1 measurement**.

| Measurement | Result |
|---|---:|
| 3840×2160 HEVC Main10, first run, 240 frames | **718.9 fps** (334 ms) |
| Same clip, three subsequent fresh sessions | **718.4 / 720.4 / 720.0 fps** (333–334 ms each) |
| Hardware versus Flicks software, six 32-frame clips | **192/192 frames**, maximum Y/Cb/Cr sample error **0** |
| Hostile configuration/sample parser cases | **8,647**, no escaping panic/crash |
| Damaged compressed access units / drop with work in flight | **8 rejected / 32 clean drops** |

### Method and scope

- `flicks-testkit` locates external ffmpeg; libx265 generates a 240-frame, 60 fps
  `testsrc2` MP4 at 3840×2160, `yuv420p10le`, fast preset, three B-frames, fixed
  24-frame keyframe interval, limited-range BT.709. The compressed access units
  total 14,369,647 bytes. Fixture generation and demuxing are outside the timer;
  media is temporary, never committed.
- Each timed run creates a **new hardware-required** VideoToolbox session, submits
  all access units with their MP4 presentation timestamps, drains asynchronous
  output and flushes delayed frames. It checks exactly 240 monotone-PTS frames.
  The timer includes configuration parsing, session creation, compressed input
  copies, callbacks, presentation reordering and flush. Decoded NV12/P010 buffers
  are retained, checked for IOSurface backing and released without CPU pixel
  copies or rendering. At most eight submissions are in flight. This measures
  decode throughput, not disk/network I/O, GPU presentation or A/V playback.
- The correctness matrix uses H.264 8-bit, HEVC 8-bit and HEVC 10-bit, each in
  limited and full range, with B-frames, 318×242 visible dimensions (coded padding),
  a GOP boundary and timestamps offset by −1,000 ticks. Hardware frames remain
  readable after their decoder is dropped. Test-only plane copies remove padding,
  apply the visible crop, deinterleave CbCr and shift P010 down by six bits before
  comparison with `flicks-h264` / `flicks-hevc`. **Tolerance is zero**: both paths
  reconstruct integer samples, with no colour conversion or perceptual comparison.
- Hostile cases include every configuration/sample prefix, 4,000 deterministic
  configuration-byte mutations, empty/oversized AUs, bad NAL lengths and forbidden
  header bits, truncated compressed pictures, terminal-error behaviour and dropping
  sessions with pending callbacks. This is bounded regression coverage, not a
  guarantee about defects inside Apple's decoder. Existing SPS-parser arithmetic
  panics are contained with `catch_unwind`; the workspace uses unwind, not abort.

### Reproduce

```sh
CARGO_PROFILE_RELEASE_STRIP=none FLICKS_REQUIRE_ORACLES=1 cargo test -p flicks-platform --release -- --include-ignored --nocapture
cargo clippy -p flicks-platform --all-targets --target aarch64-unknown-linux-musl --locked -- -D warnings
FLICKS_REQUIRE_ORACLES=1 scripts/check.sh
```

All three platform release tests passed. The full workspace gate passed formatting,
Clippy with warnings denied, the dependency policy, **731 tests/doctests** (26
existing or benchmark ignores). Linux all-target Clippy cross-check passed; Linux
returns `Unsupported` rather than linking Apple frameworks. No Linux runtime or
M1 hardware test was performed.
