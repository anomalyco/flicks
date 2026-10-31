# Performance measurements

## FLK-47 — adaptive playback over signed HLS (2026-10-31)

Apple M5 Max, macOS 27.2 (26B5112a), rustc 1.99.0, release/LTO, code `88bf711`.
Both sources **returned to original quality with zero rebuffers and zero missing or dropped
frames**. These are 210-second playback windows per source, not whole-episode runs.

| Source / audio clock | First frame | Rebuffers / time | Switches | Frames presented / dropped | Max absolute A/V offset (all / at switches) | Peak compressed buffer | Peak sampled RSS |
|---|---:|---:|---:|---:|---:|---:|---:|
| 3840×2160 HEVC Main10, silent AAC / real CoreAudio | 875 ms | 0 / 0.000 s | 6 | 5,019 / 0 | 10.495 / 6.060 ms | 33.4 s / 41.3 MiB | 85 MiB |
| Original 1080p H.264/AAC episode A / null sink | 670 ms | 0 / 0.000 s | 3 | 5,024 / 0 | 6.324 / 4.143 ms | 34.8 s / 16.4 MiB | 87 MiB |

| Media seconds displayed | 480p | 720p | 1080p encode | Direct | Final return to Direct (wall time) |
|---|---:|---:|---:|---:|---:|
| Generated 4K | 52.00 | 76.00 | 4.00 | 77.08 | 184.9 s |
| Episode A | 56.33 | 0 | not offered | 152.96 | 145.8 s |

**Method.** A real `flicks serve` under temporary HOME, no TMDB key, paired viewer, signed
requests over pinned TLS. A loopback TCP proxy shares **one downstream** among all connections.
It changes rate on wall time: 20 Mbps at open, 8 at 35 s, 3 at 70 s, 8 at 105 s, 20 at 140 s.
With a forward buffer, a change in the *displayed* rendition trails the bandwidth change.
The 4K stream showed 480p → Direct → 1080p → 720p → 480p → 720p → Direct; episode A showed
480p → Direct → 480p → Direct. Its 3.56 Mbps original fits the 8 Mbps link. At the end of
the 3 Mbps phase the EWMA was 3.16–3.17 Mbps; at the end of the last phase, 20.07–20.24 Mbps.
Transferred including TLS/framing/prefetch: 168.9 / 88.6 MB. No tokens or test-only tier forcing.

The generated fixture is a 240 s MP4: 5,760 frames at 24 fps, 10.80 Mbps video, closed 4 s
GOPs, fresh continuous 48 kHz AAC. Episode A is the original 1,203,733,864-byte file from the
read-only TV folder; it was neither copied nor modified. A 120 Hz caller drains owned frames,
without waiting for the decode worker. **Every displayed PTS** was compared in order against
the original progressive MP4 sample table with its edit offset; every frame size matched the
reported tier. The worst display poll **on a switch** was 9.458 / 1.042 µs. Across *all* polls
(including CoreAudio start) the maxima were 47.594 ms / 85.625 µs. Both players sought from
the end of the measured window back to 37.5 s and displayed PTS 37.500: 460 / 602 ms.

A/V here means frame release versus the player's audio clock, **not physical scanout or acoustic
latency**. The episode's null sink decodes the actual audio and advances only through buffered
samples; the generated case used a device with silent input. Sampled `ps` RSS every 500 ms
includes the player, proxy and harness in one process, not the server. It is not a bound on
GPU/driver memory. No UI, display-link, physical LAN, or WAN benchmark is implied.

**Policy and limits.** A download is still a local file. A signed master with no encode, HLS
setup returning 404/422/503, or the 20 s setup deadline keeps signed byte ranges. For a LAN candidate (loopback, private
or link-local IP, localhost or `.local` name), a signed 256 KiB read must sustain **both
50 Mbps and 3× original average bitrate** to stay on byte ranges; the player abandons this
probe as soon as it cannot qualify. Otherwise use HLS. Start at the smallest tier. Select the
highest average bitrate ≤70% of the EWMA of video-body transfers (50% new sample; exclude
encode/header latency and bodies below 16 KiB). Upgrades need ≥10 s buffered and two confirming decisions; below 6 s,
step down, without waiting for upgrade confirmation. Direct must start on an IDR and reach the
next cut in presentation time; an unsafe open GOP stays on encoded video rather than losing
pictures. Decoder changes drain delayed frames and keep the selected stereo AAC decoder, PCM
queue and clock. AAC ticks must be contiguous across cuts; seeks discard the previous region.

Fetch stops at ~30 s ahead (plus at most one cut). The active cut and queue share a **72 MiB**
compressed-byte/index budget, separate from the existing short decoded-frame buffer. Only one
additional cut can be in flight: video response ≤65 MiB and stereo ≤2 MiB; ≤8,192 samples per
fragment. Init ≤1 MiB, playlists ≤8 MiB (master ≤64 KiB). Malformed input/auth errors are not
retried as another URL. `Player::stats().adaptive` is `None` on byte ranges/local files, otherwise
it reports the **displayed** `tier`, `throughput_bps`, `buffer_seconds` and `buffer_bytes`.

**Retained LAN path, before → after.** On FLK-5's same 16 s, 11,570,581-byte separated-track
fixture and pinned loopback, `5db408c` → `88bf711`: 46 → **48** TLS requests (master and range
probe added), open to decoded preroll **74.51 → 85.63 ms**, four seek-to-frame times
**12.22–14.03 → 12.61–13.97 ms**, 384/384 frames and zero drops both times. Single standalone
release runs, warm filesystem, not a statistically significant latency comparison.

```sh
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_STRIP=none
export FLICKS_REQUIRE_ORACLES=1
cargo build --release --locked -p flicks-server
# Port 7686 must be free; macOS with CoreAudio. Without the optional path only the 4K case runs.
FLICKS_PLAYER_MEDIA=/absolute/path/to/episode.mp4 cargo test --release --locked -p flicks-player adaptive_throttle -- --ignored --nocapture --test-threads=1
cargo test --release --locked -p flicks-player network_ranges_and_seeks -- --nocapture --test-threads=1
scripts/check.sh
cargo zigbuild --locked -p flicks-server -p flicks-player --target aarch64-unknown-linux-musl
```

Workspace gate: format, Clippy with warnings denied, dependency policy, **836 tests/doctests
passed** (38 ignored). Includes malformed/truncated playlists, sequence/size/offset/timestamp
mutations in CMAF, signed retry and path rejection, audio-tail silence, and existing oracle
and fuzz suites. Linux ARM64 musl cross-build passed; no non-macOS runtime or encoder measured.

## FLK-26 — on-demand macOS HLS transcode (2026-10-30)

Apple M5 Max, macOS 27.2 (26B5112a), rustc 1.99.0, release with LTO. Server `342e4e1`,
acceptance harness `7e4e4f9`. Previously there were no transcoded renditions, and HLS required
MKV. These measurements include the complete signed, pinned loopback HTTPS request to a real
`flicks serve`: catalog lookup, reopen/seek, decode, transfer, encode, fMP4 mux and response body.
They are warm-filesystem measurements, **not cold-disk, Internet or whole-film benchmarks**.

| Source → H.264 SDR tier | Uncached segment ms (3 distinct cuts) | × realtime | Immediate LRU repeat ms | Decoded PSNR, one cut (dB) |
|---|---:|---:|---:|---:|
| 4K PQ closed GOP → 1080p | 434–454 | 8.85–9.22 | 2.4–3.0 | 39.97 |
| 4K PQ closed GOP → 720p | 312–323 | 12.39–12.84 | 1.5–1.6 | 38.75 |
| 4K PQ closed GOP → 480p | 195–244 | 16.38–20.55 | 1.4–1.5 | 37.91 |
| 4K PQ open GOP → 1080p | 422–448 | 8.97–9.48 | 2.3–2.6 | 39.98 |
| 4K PQ open GOP → 720p | 260–289 | 13.91–15.38 | 1.5–1.6 | 38.77 |
| 4K PQ open GOP → 480p | 229–293 | 13.74–17.51 | 1.2–1.9 | 37.87 |
| Real 1080p MP4 → 720p | 225–246 | 17.82–18.32 | 1.1–1.6 | 44.56 |
| Real 1080p MP4 → 480p | 142–156 | 28.27–28.64 | 1.2–1.3 | 44.05 |

The generated clips were 16 s / 384 pictures at 24 fps, 3840×2160 Main10 PQ/BT.2020, 5.1 AAC,
4-second x265 GOPs, 18.6 Mbps files. Cuts 2, 1 and 3 lasted 4.000–4.021 s. Cut 0 was also
verified, but **excluded from encode timings**: obtaining its `avcC` for `/init.mp4` already
produced it. At 8/3/1.2 Mbps video targets the busy test pattern actually used about 7.0/2.62/1.05
Mbps, not counting AAC. The read-only original episode A was a 1,203,733,864-byte H.264/AAC
MP4 (about 45 min, 3.56 Mbps including audio). It offers 720p/480p, not a same-size 8 Mbps tier.
Cuts 299, 1 and 597 lasted 4.000–4.458 s. No source conversion, copying, or modification was
needed to serve the episode. Each repeat was byte-identical, and init bytes matched across
independent encoder runs for each tier.

**Playback.** Headless Swift AVPlayer decoded video at start and after seeks to 9 s (generated)
or 1,353.5 s (episode). **26/26 phases, 0 stalls, 0 error-log events**, nonempty access logs:
three forced SDR video+stereo-audio variants per generated source, two for the episode, original
video for all three, plus both intact HDR masters. The Mac's external displays reported HDR
ineligible: the intact masters correctly selected 1080p SDR. To inspect 4K original PQ video
on this machine the test requested its real **video media playlist**; it did not pretend that an
HDR-only master was selectable on an SDR display. The original episode used its normal A/V
variant. This is decoded-buffer and log evidence, not an HDR-display visual or listening test.
Episode seek completion: 67 ms direct, 1,282 ms at 720p and 842 ms at 480p in this run; not a
sub-300 ms transcode-seek claim. AVPlayer used five-minute grants through a test-only loopback
HTTP adapter to the real HTTPS server. Every forwarded request returned 200.

**Alignment and quality.** All ladder playlists had identical `EXTINF`s: 4 cuts per PQ source,
598 for the episode. On **all** generated cuts and all three sampled episode cuts, FFprobe's
source packet count matched the number of decoded output pictures exactly, every transcoded
fragment began with IDR, and its first PTS matched the direct keyframe within one source frame.
This includes three leading pictures following a CRA despite an SPS reorder bound of two, and
two pictures at the same PTS in the original MP4 tail. For that collision only, distinct encoder
decode ticks and signed fMP4 composition offsets preserve both original presentation times.

FFmpeg 9.0.2 (test-only) compared the decoded video of the middle cut with an independently
demuxed and scaled source: Lanczos for SDR, `scale_vt` with BT.709 color matching for PQ. The
latter exercises the same Apple color transform, **not an independent tone-map algorithm**.
VTPixelTransferSession does the conversion into the compression session's NV12 pool without
a CPU pixel readback. A separate neutral PQ ramp at 1/10/50/100/203/400/1000/4000 nits gave
limited-range 8-bit SDR luma **40/78/138/178/213/230/234/234**: highlight compression/clipping,
not simply relabeling PQ. Every decoded tier was 8-bit BT.709 H.264 at its advertised dimensions.
For AAC the Apple encoder reported 2,112 priming samples. The decoded 44.1/48 kHz impulse landed
at sample 1,700 in both trim tests. The 5.1 fixture's left/right notes stayed separate and the
center note appeared on both output channels. Audio stays at the supported source rate; each
cut uses the same absolute 1,024-sample AAC grid, not a separately rounded duration.

**Bounds.** Four HLS work slots, at most two encodes, 12 s HTTP deadline (permits stay with a
worker until it actually exits a native call). Up to 32 s per transcoded cut, 2,048 submitted
video frames, 4K-class source pixel budget, 16 MiB maximum source sample, 64 MiB retained
segment payload. MKV also has a 256 MiB scan budget and bounded cue-only seeks; progressive
MP4 has a 32 MiB `moov` / 8M sample ceiling and rejects fragments. The per-server LRU holds up to
16 token-free resources / 32 MiB. Auth, catalog visibility, containment, open and file identity
checks precede cache access. This isn't process isolation against a hung or defective OS driver.
The macOS hardware AVC encoder and AudioConverter are the only backends; Linux VA-API and
Windows Media Foundation encoding remain follow-ups.

**Direct-remux baseline.** The existing unoptimized FLK-19 closed-GOP test still checks all
video hashes. Its published repeat p95 → this workspace gate (ms, 20 repeats per codec pair):
H.264/AAC 12.116 → 7.720; HEVC/AAC 8.768 → 13.711; H.264/AC-3 8.997 → 10.313; HEVC/E-AC-3
8.363 → 6.817. Mixed shared-host timings, not a general improvement or regression claim.

```sh
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
export FLICKS_REQUIRE_ORACLES=1 CARGO_PROFILE_RELEASE_STRIP=none
# Port 7686 must be free. Omit FLICKS_TRANSCODE_MEDIA for generated sources only.
FLICKS_TRANSCODE_MEDIA=/absolute/path/to/episode.mp4 \
  cargo test --release --locked -p flicks-server --test server transcode_ladder_avplayer -- --ignored --nocapture --test-threads=1
scripts/check.sh
cargo zigbuild --locked -p flicks-server --target aarch64-unknown-linux-musl
```

The workspace gate passed formatting, Clippy, dependency policy and **830 tests/doctests**
(37 ignored). It includes MP4 truncation/header mutations, bounded MKV reads, platform
input rejection, cache revision/recency, and existing parser/codec fuzz suites. The Linux
ARM64 musl server cross-build and container's wasm32 check passed; no non-macOS encoder ran.

## FLK-51 — fallback artwork (2026-10-30)

Code `a25a30e`. Apple M5 Max, macOS 27.2, rustc 1.99.0. Release `flicks` worker, one software-decoder
thread, Unix nice 19. Before: without a TMDB key, both episodes had no artwork and no
frame-generation work. Now: JPEGs are generated after catalog publication by a separate
serial worker; no decoding runs on the scanner or request path.

| Read-only 1080p H.264/AAC MP4 | First real server scan, still work (ms) | Five worker runs (ms) | Median (ms) | JPEG bytes |
|---|---:|---|---:|---:|
| Episode 2, 1,203,733,864 bytes | 119 | 256, 122, 119, 128, 145 | 128 | 60,443 |
| Episode 6, 1,455,959,793 bytes | 195 | 208, 197, 196, 197, 193 | 197 | 105,881 |

Both outputs are **1280×720**, quality 85 using `image` 0.25.10's pure-Rust JPEG encoder
(MIT OR Apache-2.0, already locked through Rune). Times include spawning/reaping the
worker, container parsing, seeking an indexed keyframe near 20% (alternatives 25/15/30%),
software decoding, luma/variance rejection, conversion/scaling, encoding and atomic rename.
The 10 ms child-exit check is included. File/OS caches were warm; not a cold-disk claim.
Per-file limits: 5 seconds wall time (supervisor kills/reaps), 96 MiB total reads, 16 MiB
single read/sample, 128 GiB source size, four candidates, output at most 1280×720.
Existing container/decoder allocation limits also apply; decoding is process-isolated.

Verified with actual `flicks serve` under a temporary HOME, no TMDB key, over
`/Users/vogel/dev/flicks-test-media/TV` read-only. Restart reused both JPEGs without changing
their mtimes (zero decode logs); removing the configured root deleted both files. A paired
release desktop app from the FLK-48b lane showed the real frame in Home's hero and distinct
frames in both episode rows. Screenshots outside the repository:
`/Users/vogel/dev/flicks-brand/stills-home-check.png` and
`/Users/vogel/dev/flicks-brand/stills-check.png` (no links, keys or addresses).

The opt-in existing server integration test repeats the frame work five times, uses a
temporary catalog/artwork directory, and checks authenticated artwork response bytes:

```sh
cargo build --release -p flicks-server
env -u FLICKS_TMDB_KEY \
  FLICKS_STILL_EXE="$PWD/target/release/flicks" \
  FLICKS_STILLS_MEDIA=/Users/vogel/dev/flicks-test-media/TV \
  cargo test -p flicks-server --test server stills_real_media -- --ignored --nocapture
```

The same temporary-HOME server check with the securely injected TMDB key matched both
episodes, preserved both TMDB **series** backdrops and generated zero local still files.
Episode-specific TMDB still lookup was not present in that baseline metadata API; this initial check
does not claim episode-still precedence. Synthetic coverage exercises all four software
codecs, 16 truncations, black video, audio-only media and a black-first/good-second retry.
`scripts/check.sh` passed; a locked Linux server build in `rust:1-bookworm` passed.

### Separate episode stills and cropped bars — `cb2e508`

Same machine, release profile, read-only episodes and measurement command above. The
worker now removes near-black border rows/columns in normalized luma before the 16:9
crop and scale. `still-v2-<oshash>.jpg` regenerates the earlier letterboxed cache once.

| Episode | Real server generation before → after (ms) | Five cropped worker runs (ms) | Median before → after (ms) | JPEG bytes before → after |
|---|---:|---|---:|---:|
| 2 | 119 → 123 | 313, 115, 118, 114, 114 | 128 → 115 | 60,443 → 63,508 |
| 6 | 195 → 199 | 204, 198, 193, 194, 195 | 197 → 195 | 105,881 → 118,019 |

Both remain 1280×720. These small timing differences are within shared-machine noise,
not a speedup claim. Restart kept both cropped JPEG mtimes unchanged. A release desktop
built from this lane showed both distinct, bar-free episode frames; the updated screenshot
is `/Users/vogel/dev/flicks-brand/stills-check.png`.

Restarting that same temporary-HOME server with the securely injected key replaced both
local frames with **distinct TMDB episode stills**. Each `still` matched its live episode API
response, while `poster` and `backdrop` still matched the series API response. Both obsolete
local JPEGs were removed. The release desktop showed the series hero and separate episode
images together: `/Users/vogel/dev/flicks-brand/stills-tmdb-check.png`. All screenshots omit
links, keys and addresses and stay outside the repository.

A simulated settled match with a missing episode image generated a local still while
preserving the real TMDB series backdrop; removing its root removed that fallback JPEG.
Existing tests cover null/404 stills, migration, best-source merge, API and UI requests,
plus unpadded/top-bottom/side/combined-border frame comparisons. Final foreground
`scripts/check.sh` and the locked Linux server build in `rust:1-bookworm` passed.

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
## FLK-5 — player integration and acceptance gap (2026-10-30)

Apple M5 Max, macOS, release Rust builds, generated media through `flicks-testkit`'s
external ffmpeg. **Handed off for app integration, not a completed two-hour acceptance
claim.** The normal `scripts/check.sh` gate passes at player commit `168736c`.

### Current engine (`168736c`)

Hardware frames retain their IOSurface and visible crop; no CPU crop copy is needed.
Audio uses `Output::position()` directly because that API already accounts for device
latency, queues about 300 ms, and preserves queued audio across pause/resume. The null
sink decodes all audio and advances a caller-driven clock only through buffered audio.

| Check | Result |
|---|---|
| Two independent 1080p H.264 players, 60 seconds wall-clock paced, 120 Hz caller, null sinks | 1,440 frames displayed each; **0 drops**, maximum release A/V offset **6.053 ms** |
| Display call cost in that run | p50 **2.167 µs**, p99 **1.383 ms**, maximum **5.420 ms** |
| Maximum caller scheduling gap in that run | **13.472 ms** |
| Real CoreAudio output, 3-second H.264/AAC MP4, including 100 ms pause/resume | **72/72** displayed, **0 drops**, maximum release A/V offset **5.328 ms**; clock reached 3.000 s |
| Normal player tests | **6 pass**: software codecs/audio layouts, seek/pause, track/subtitle selection (including indexed cues spanning a seek), fallback replay/drop counting, hostile/truncated files, signed TLS read-ahead/seek and verified download playback after stopping the server runtime |

The 60-second run used the first minute of the generated two-hour MP4 below. The
caller feeds owned frames into the void, not a window/GPU pass. Timing measures release
against the audio clock, not compositor presentation latency or an acoustic loopback.

### Earlier measurements (before the crop API was available)

Player `71afe9c` and its test-harness follow-up `e541e7a` copied cropped hardware buffers
to CPU planes. These numbers are retained as the baseline, not substituted for current
long-run evidence:

| Generated file, 24 fps, AAC stereo | Wall-clock paced playback | Maximum A/V offset | Local seek including first display frame, three seeks |
|---|---|---:|---:|
| 1080p H.264 MKV, 10 s | 240 displayed, 0 dropped | 1.392 ms | 10.5–11.7 ms |
| 1080p H.264 MP4, 10 s | 240 displayed, 0 dropped | 1.045 ms | 11.3–12.0 ms |
| 4K HEVC Main10 MKV, 10 s | 240 displayed, 0 dropped | 1.371 ms | 26.5–28.3 ms |
| 4K HEVC Main10 MP4, 10 s | 240 displayed, 0 dropped | 1.068 ms | 25.8–26.6 ms |

- Local Flicks server, real pairing and signed HTTPS range reads: four 1080p MP4 seeks
  including first frame **20–24 ms**. Background read-ahead is bounded to 8 MiB, in
  256 KiB ranges, invalidated on seek. Not a physical LAN measurement.
- One supplied 1080p H.264/AAC episode, read-only: first ten media seconds decoded in
  **112 ms** with a driven clock and zero dropped frames; seeks to 60/600/120 seconds
  including first display frame **18.3 / 31.0 / 30.5 ms**. This was not paced playback.
- 4K HEVC Main10 software path: **24 frames / 0.537 s = 44.7 fps**, including planar to
  GPU-ready conversion and AAC decode. Only a one-second generated sample, not a claim
  about arbitrary 4K material.

### Two-hour result: failed, rerun still required

Two 1080p H.264 files (MP4 and MKV), 172,800 frames each, were driven together at 120 Hz
for **7,200.002 wall-clock seconds**, decoding video and AAC and displaying into the
void with null audio sinks. The MP4 reported **172,800 decoded, 172,720 displayed,
80 dropped, maximum A/V offset 37.429 ms**, zero hardware fallbacks. The harness failed
its assertion while reporting MP4, so MKV's final counters were not printed. Both files
were driven for the duration; no successful MKV result is inferred.

The fixture loops a generated two-second H.264 video and encodes a fresh continuous
two-hour AAC track, then remuxes to MKV (about **4.8 GiB each**). Pinning 172,800 video
frames is necessary: ffmpeg stream-copy with `-t 7200` alone retained two reordered tail
pictures; an early assertion caught that before playback. No media is committed.

Removing CPU cropping and merging the reviewed audio/hardware fixes precede the
successful 60-second diagnostic above. **They have not yet been shown to fix the
two-hour failure**, whose scheduling/refill cause was not measured in the original
run. The long harness now reports both players before asserting, maximum display-call
cost and maximum caller gap. Rerun it before closing FLK-5's performance criterion.

```sh
scripts/check.sh
cargo test -p flicks-player --release realtime_and_seek -- --ignored --nocapture
cargo test -p flicks-player --release device_clock_pause_resume -- --ignored --nocapture
cargo test -p flicks-player --release software_4k_throughput -- --ignored --nocapture
# Existing media is read-only; the diagnostic writes no files:
FLICKS_PLAYER_MEDIA=/absolute/path/to/video.mp4 cargo test -p flicks-player --release scheduling_diagnostic -- --ignored --nocapture
# Generates ~10 GiB, then runs for two wall-clock hours:
cargo test -p flicks-player --release two_hour_playback -- --ignored --nocapture
```

### Review fixes — bounded decode worker and retained range cache (2026-10-30)

Measured on the same M5 Max in release, before at `1f90f61` and after at
`3c355d7`. Open and seek now wait for the initial ~350 ms decode buffer;
`next_frame_for_display` polls a one-request/one-batch worker instead of reading
or decoding media on the caller. Public method signatures are unchanged.

| Measurement | Before | After |
|---|---:|---:|
| Two 1080p H.264 players, 60 s, 120 Hz: display call p50 | 2.250 µs | 0.833 µs |
| Same run: display call p99 / max | 1.530 / 6.253 ms | 0.025 / 0.108 ms |
| Same run: max caller gap | 13.391 ms | 12.566 ms |
| Same run: displayed per player / dropped | 1,426 / 0 | 1,425 / 0 |
| Same run: max A/V offset | 12.807 ms | 9.350 ms |
| 16 s, 384-frame, 11,570,581-byte track-separated MP4: signed range requests / TLS connections | 1,537 | 46 |
| Local seek incl. first frame, 1080p H.264 MKV (three seeks) | 10.33–11.09 ms | 9.67–10.87 ms |
| Local seek incl. first frame, 1080p H.264 MP4 | 15.16–22.49 ms | 10.11–11.66 ms |
| Local seek incl. first frame, 4K HEVC Main10 MKV | 26.68–27.14 ms | 25.26–27.37 ms |
| Local seek incl. first frame, 4K HEVC Main10 MP4 | 27.93–33.93 ms | 25.34–27.02 ms |

The paired 60 s runs use the **same** 62-second fixture, stream-copy looping a
two-second H.264/AAC seed. Its repeated AAC priming gaps mean it is not exactly
24 displayed frames per wall-clock second; do not compare its frame count to the
continuous-audio 1,440-frame diagnostic above. Neither run dropped a frame. The
one-frame endpoint difference is retained rather than normalized away. All four
fresh continuous-audio 10 s seek fixtures displayed 240/240 with zero drops;
after max A/V offsets were 2.033 / 1.071 / 1.836 / 3.419 ms in table order.

Range counts include open, metadata and all playback reads, through a loopback TCP
proxy to the real paired, signed HTTPS server (one request per TLS connection).
Video and audio are stored in separate contiguous regions >8 MiB apart at the
start, but consumed in DTS order. The baseline measurement temporarily restored
the old sliding-window eviction policy with the new harness/worker; its <100
request regression assertion failed at 1,537. Restoring the 32-block LRU passes
at 46. Cache hits retain both tracks; only an explicit seek invalidates blocks.
Each active track also requests its next block speculatively, even with a full
LRU; speculative reads do not run unboundedly ahead and displace the other track.
Range failures retry once; the redundant 10 s whole-response timeout is removed,
leaving the client's connect/header and idle-byte timeouts. Local files use a
256 KiB buffered reader, including relative seeks within the buffered region.

Stalling new TCP connections in that signed-server test produces `Buffering`
without waiting in the display call. With decoded/null audio, display p99/max
was 39.833/66.500 µs; with audio deselected it was 108.208/114.208 µs. The extra
poll while blocked measured 0–41 ns. Both resumed after a
five-second driven-clock gap, displayed 384/384, dropped zero and reached
`Ended`. Assertions require p99 and the blocked poll below 1 ms. A deliberately
dropped first connection also verifies the single range retry.

EOF regression cases cover an enabled empty MP4 audio track and 209 ms / 5 ms
video tails: all 24 frames display, zero drops, and `Ended` is reached. Known
exhausted MP4 audio tracks permit a wall-clock tail bounded by decoded video;
other containers use demux EOF. `NoDevice` selects the null sink; other output
errors remain errors. The real CoreAudio pause/resume test displayed 72/72,
zero drops, max offset 3.685 ms, ending at 3.000 s.

Verification: nine normal release player tests pass; the release 60 s, seek and
real-device tests pass. Workspace Clippy (`-D warnings`), dependency policy and
all **799 tests/doctests** pass (35 ignored) with required external oracles.
The final player-only rerun also covers seeking away from an unobserved failed
worker batch; player-only Clippy and formatting pass after that small follow-up.
`scripts/check.sh` itself stops at the pre-existing extra final blank line in
`apps/desktop/src/worker.rs` from the base commit. That unowned file is unchanged;
the remaining gate commands were run explicitly in the foreground.

The two-hour harness now calls `sync_all` on both generated files before opening
players and paces each player on its own thread. **The two-hour acceptance is
still pending**, not inferred from shorter runs: at 18:13 the 18-core Mac's load
was 24.44 / 19.72 / 16.53, other `rustc`/fuzz jobs were active, and a VM was using 305% CPU.
This was not a reasonably quiet machine for the acceptance run. Run later:

```sh
cd /Users/vogel/dev/flicks-fix-player
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  cargo test -p flicks-player --release two_hour_playback -- --ignored --nocapture
```


## FLK-19 review — HLS tokens, open GOPs and audio clocks (2026-10-30)

Apple M5 Max, macOS 27.2; default unoptimized test profile (including the
workspace's per-package optimizations). Same pinned-loopback HTTPS measurement
as below: first signed video-segment request, then 20 byte-identical repeats,
after scanning and fetching the master. Warm filesystem; no AVPlayer run before
either timed matrix. Before is `1f90f61`; after is the `fix-hls` review fix.

| Generated input | First ms, before → after | Repeat p50 ms, before → after | Repeat p95 ms, before → after |
|---|---:|---:|---:|
| H.264 / AAC | 9.314 → 12.061 | 8.786 → 8.634 | 10.846 → 12.116 |
| HEVC / AAC | 7.888 → 8.764 | 8.354 → 8.150 | 9.944 → 8.768 |
| H.264 / AC-3 | 7.879 → 8.668 | 8.527 → 8.468 | 9.173 → 8.997 |
| HEVC / E-AC-3 | 7.611 → 7.572 | 9.741 → 7.876 | 21.242 → 8.363 |

These shared-host samples do not establish a general speedup. Signed-request
timings still include P-256 authentication. Native-player grants now use
HKDF-derived HMAC-SHA256 instead: forged tokens do no SQLite work, verified by
holding the DB thread blocked while 25 requests finish (20 unauthorized, then
five rate-limited). Valid grants still check the device for immediate revocation.

Correctness and native playback:

- Six generated cases: the four closed-GOP combinations above, x265 open-GOP
  HEVC/AAC, and x264 open-GOP H.264/44.1 kHz AAC. All full-playback and eight-second
  seek video hashes equal the source; each open-GOP final fragment also decodes
  independently to the exact source tail. Every segment is byte-stable on
  reverse-order repeated requests. Generated audio SimpleBlock keyframe bits are
  cleared: output audio is still entirely sync, uses the sample-rate timescale,
  and AAC durations are exactly 1024 samples.
- Keep CRA/recovery and leading pictures together in decode order; moving RASL
  to the preceding segment would remove its CRA reference. Subtract the leading
  picture count from the next fragment's decode-time anchor and mark those
  samples as leading. The decoder suppresses pre-recovery output on random
  access. **Do not advertise `EXT-X-INDEPENDENT-SEGMENTS`**: RFC 8216 requires all
  samples to be independently decodable, which dependent leading pictures are
  not. The catalog cannot distinguish IDR from CRA, so it cannot prove this tag.
- Audio timestamp error at start / eight seconds: 48 kHz AAC 0.353 / 0.353 ms;
  44.1 kHz AAC 0.232 / 0.228 ms; Dolby 5.353 / 26.647 ms (32 ms frames).
  x264 open-GOP seeks produce the same FFmpeg `mmco: unref short failure` warning
  on the source MKV; pixel comparisons still match. HEVC seeks have no errors.
- Headless Swift AVPlayer, using the test-only loopback HTTP adapter and real
  token URLs: **6/6 ready, played, sought to eight seconds, and decoded video
  after the seek**. AVPlayerItemVideoOutput returned 44–45 buffers per case,
  including 23 after seeking; zero AVPlayer error-log events. The helper mutes
  playback. This is native decoding, not a Safari visual or listening test.
  `mediastreamvalidator` is not installed. No real-episode rerun is claimed.
- Total scanned payload is bounded at 256 MiB (including unselected tracks),
  retained rendition payload at 64 MiB, plus the existing packet-count limits.
  Audio's earlier-cue preroll has a separate scan allowance for high-bitrate
  video. Dolby config reads start at segment preroll for Dolby `.m4s`, not at
  cluster zero; unrelated renditions do not read Dolby frames.
- Existing codec SPS parsers replace the duplicate container parsers; 603
  truncations/bit flips and a no-VUI-restriction DPB regression pass. Container
  remains codec-independent and its `wasm32-unknown-unknown` check passes.

Verification: `FLICKS_REQUIRE_ORACLES=1 scripts/check.sh` passed 800 tests and
doctests (35 ignored), Clippy and dependency policy after temporarily normalizing
an existing extra EOF blank line in `apps/desktop/src/worker.rs`. That unrelated
file was restored, so the branch's full formatting gate still reports that
baseline issue. Final targeted server/container Clippy and HLS/native checks
pass. Use the existing `hls` test filter for the two unit regressions and the two
integration tests; optional `FLICKS_HLS_AVPLAYER=/absolute/path/to/helper.swift`
runs a local Swift playback helper with each generated token URL.

## FLK-19 — On-demand MKV to HLS remux (2026-10-30)

Apple M5 Max, macOS 27.2 (26B5112a), Rust 1.99.0. **Unoptimized test profile**,
debug info and incremental compilation disabled; not a release or M1 claim.

| Input | First video segment bytes | First response ms | Repeat p50 / p95 / max ms |
|---|---:|---:|---:|
| Generated H.264 / AAC | 120,935 | 6.265 | 6.316 / 6.906 / 6.970 |
| Generated HEVC / AAC | 92,279 | 6.551 | 6.265 / 6.891 / 7.155 |
| Generated H.264 / AC-3 | 120,935 | 6.397 | 5.631 / 5.842 / 5.931 |
| Generated HEVC / E-AC-3 | 92,279 | 5.819 | 5.939 / 6.332 / 6.333 |
| Real episode A, H.264 / AAC | 2,201,948 | 13.157 | 12.305 / 12.998 / 14.275 |
| Real episode B, H.264 / AAC | 2,476,539 | 16.021 | 16.459 / 30.365 / 158.639 |

### Method and correctness

- First request plus 20 repeats of video segment zero over pinned loopback HTTPS.
  Timing includes request signing, fresh TCP/TLS connection, metadata parsing,
  cue seek, packet reads, muxing and complete response consumption. The catalog
  scan and master-playlist request happen first, so filesystem data is warm;
  “first” does **not** mean cold disk. There is no segment cache. All first
  responses measured below 50 ms; the 158.639 ms repeat outlier remains reported,
  not discarded. These shared-host samples are not a latency guarantee.
- Four generated 12-second 320×180/24 fps B-frame fixtures: H.264/AAC,
  HEVC/AAC, H.264/AC-3 and HEVC/E-AC-3. Two read-only local MP4 episodes supply
  temporary 16-second stream-copy MKVs; originals are unchanged. Production
  never invokes FFmpeg or writes media files. Source MP4 demux is not an HLS input.
- FFmpeg decodes HLS at start and after a relative eight-second seek. All video
  frame hashes match the MKV source exactly, including 288 full-playback frames
  per generated fixture. Audio frame timestamps, after removing the shared
  reorder delay, differ by 0.334 ms for generated AAC and at most 26.666 ms for
  Dolby (32 ms frames); real AAC seek errors are 0.720 and 0.185 ms (limit 23 ms).
  This checks audio timing and successful decoding, not bit-identical PCM:
  pre-zero priming frames are deliberately dropped, without edit lists.
- Every advertised video/audio segment is fetched twice in reverse order,
  parsed and required nonempty; 20 repeated first-segment bodies also match
  byte-for-byte. Episode B exposed a 209 ms video-only final cut: keeping a
  sub-second tail in the preceding segment fixes the empty audio response.
- Oracle playback uses a test-only loopback HTTP adapter to the same authenticated
  handler because FFmpeg does not propagate its TLS option to child playlists.
  Performance, signature checks and grant-revocation checks use real HTTPS.
  No AVPlayer or Apple `mediastreamvalidator` result is claimed (validator absent).
- Writer/SPS/sample fuzz coverage: 10,000 randomized cases plus 2,502 truncations
  and bit flips. Container `wasm32-unknown-unknown` check passes. Final workspace
  gate passes formatting, Clippy, dependency policy and **776 tests/doctests**
  (29 existing/benchmark ignores), including the four-codec HLS oracle matrix.

### Reproduce

```sh
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
export FLICKS_REQUIRE_ORACLES=1 CRAFT_MEDIA_REQUIRE_ORACLES=1
cargo test -p flicks-server --test server hls_oracle -- --nocapture
# Optional read-only source; the test remuxes a short copy into its temporary dir:
FLICKS_HLS_MEDIA=/path/to/local-episode.mp4 cargo test -p flicks-server --test server hls_oracle -- --nocapture
cargo test -p flicks-container --test fuzz cmaf_ -- --nocapture
cargo check -p flicks-container --target wasm32-unknown-unknown
scripts/check.sh
```

## FLK-48 — app motion pacing (2026-10-30)

Apple M5 Max, macOS 27.2, release profile, code `60fcaac`, Rune `e450c92`. The app window on
an LG 5K external display, which runs at **60 Hz** (the trace header's `hz`; this desk has no
ProMotion screen). The 120 Hz run on the MacBook (exonbook) is still to do: it was offline (not on
the tailnet, no mDNS answer) during this card.

Each phase is the real `flicks-ui` app in a native window with `--native --trace`, its motion
driven 4 times there and back (8 bursts) through `AppHandle::update`: a Continue Watching card's
lift (scale spring 0.32 s plus shadow), the row gliding 1,200 pt (`tx.scroll_to`), Home to a
title's page and back (the screen crossfade with scale, the title page's blurred backdrop), and
the page scrolling 560 pt under the hero (the top bar's frost turning solid). The library is
the fixture's show plus 12 movies in progress, so the Continue Watching row overflows. Two runs:

| Phase | Frames | Present interval p50 / p95 / max (ms) | Missed refreshes (> 25 ms) | Encode CPU p50 / p95 / max (ms) |
|---|---:|---:|---:|---:|
| Tile lift | 313, 315 | 16.67 / 16.67 / 33.34 | 3, 3 | 1.12 / 2.02 / 3.07; 1.01 / 1.24 / 3.15 |
| Row glide | 288, 288 | 16.67 / 16.67 / 33.34 | 3, 3 | 1.39 / 2.14 / 9.42; 1.09 / 1.45 / 4.12 |
| Home ↔ title page | 464, 462 | 16.67 / 16.67 / 33.34 | 3, 6 | 1.78 / 2.77 / 7.24; 1.40 / 2.02 / 5.83 |
| Page under the hero | 278, 279 | 16.67 / 16.67 / 33.34 | 5, 3 | 1.15 / 2.14 / 9.83; 0.99 / 1.55 / 2.12 |

- Every motion runs on the display link at the display's rate and stops when it settles: each
  burst ends 0.6 s (lift, row, scroll) to 1.0 s (screen change) after it starts, and the window
  draws nothing between them (`pacing: idle`). No frame bunched (< 2 ms) in either run.
- Encode CPU p95 is 1.2 to 2.8 ms, a third of the 8.3 ms a 120 Hz frame has. The maxima of 6 to
  10 ms are single frames in the middle of a motion, not at a commit, and differ run to run;
  other agents' `cargo build`s shared the CPU during both runs.
- Most misses are the first frame after the immediate one that starts a motion from rest: it
  presents one refresh after its display-link target (encoded 23 ms ahead, under 2 ms of CPU).
  That repeats at the start of a third to half of the bursts, so it's on Rune's display link
  restart, not the app; noted for a Rune PR. A real click into a title's page (input, not
  `AppHandle::update`) ran 62 frames with no miss and a 16.7 ms max interval.

### Reproduce

The harness is a throwaway binary outside the repo depending on `flicks-ui` by path: it builds
the catalog above with `library::fixture::{item, title}`, opens `flicks_ui::spec()` with
`App::init`, and from a thread calls `handle.update` to set `lifted`, `tx.scroll_to("row-0-scroll",
1200.0, 0.0)`, `s.go(Screen::Detail(..))` / `s.go(Screen::Home)` and `tx.scroll_to("hero", 0.0,
560.0)`, waiting 1 to 1.6 s after each so every motion is its own burst.

```sh
flk48-bench --native --trace pacing.jsonl   # prints Rune's present summary on exit
# Per phase: split frames into bursts at gaps > 150 ms, 8 bursts per phase in order; intervals
# from present_dt_ms, CPU from quality.cpu_ms, missed = intervals > 1.5 refresh periods.
```

## FLK-6 — player screen (2026-10-30)

Apple M5 Max, macOS 27.2, release builds of `flicks-desktop` and `flicks`, Rune `e450c92`. The
app window on the 60 Hz LG 5K display (the trace header's `hz`); the 120 Hz MacBook (exonbook)
didn't resolve on the network during this card, so 5:5 at 120 Hz is reasoned, not measured. A
local `flicks serve` (temporary HOME) shared the read-only test-media TV folder over signed
HTTPS on this Mac; the app paired with it like any device. Other agents' builds and apps shared
the machine.

| Check | Result |
|---|---|
| 1080p H.264/AAC episode, 199 s with the controls and a menu showing | 4,768 displayed, **0 dropped**, hardware decode |
| Same episode in four more plays, about 6 minutes (normal viewing, seeks, a sidecar) | **0 dropped** in every play |
| 4K HEVC Main10 PQ (BT.2020) clip, 20 s, in two plays, to its end | 201 + 279 displayed, **0 dropped**, hardware decode, tone mapped to SDR |
| CPU while playing 1080p (one core = 100 %) | **6.3–8.1 %** (`top`, 10 s samples) |
| CPU paused, and with the player closed | **0.0 %**, 0 idle wakeups |
| Present interval while playing, overlay up (222 s, 11,872 frames) | p50 **16.667**, p95 16.668, p99 16.668, max 33.3 ms; **9 missed** (> 25 ms), 0 bunched |
| Encode CPU per frame while playing | p50 0.25, p99 0.55 ms |
| A session with every screen change and two plays (5,649 frames) | p50 / p95 16.67, max 116.7 ms, 5 missed |

Every playback miss was encoded about 23 ms ahead of its target with under 1 ms of CPU, so they
are on the GPU or WindowServer side, not the app's or Rune's CPU; the 116.7 ms gap is a capture
by the test driver. Rune's own summary is printed on exit with `--native --trace <file>`.

**Video cadence.** 24 fps on 60 Hz should alternate 2 and 3 refreshes a frame. Counting, per
video frame, the refreshes it stayed on screen and how often a count repeated the previous one
(a 2,2 or 3,3 is a frame shown a refresh early or late):

| Frames shown | Repeats | How frames were chosen |
|---|---:|---|
| 6,673 | 656 (9.8 %) | the newest frame the playback thread had handed over (it polls every 4 ms) |
| 1,824 | 39 (2.1 %) | the frame due by each display tick, ticks from Rune's frame clock |
| 1,825 | **2 (0.1 %)** | the same, with frames due on a smoothed audio clock (both at the start) |

The playback thread stamps each frame with when the audio clock passed it, through a media-to-
wall mapping of slope 1 that follows the clock's readings slowly (1/64 a step, snapping after
a seek, a stall or a resume); the compositor shows the newest frame due 8 ms before each tick,
reading ticks from `Runtime::t`, which steps by the display link's targets. The same rule gives
5 refreshes a frame at 120 Hz. The cost is latency: a frame reaches the screen about 8 ms plus
one to two refreshes after its audio, because `flicks-player` hands frames over only once due.

Measured with a temporary counter in the compositor (not committed); the app logs each play's
stats when it stops (`flicks: stopped at …: N decoded, N displayed, N dropped, …`).

```sh
HOME=$TMP/server flicks serve &                     # then folder add, user add, pair
HOME=$TMP/app flicks connect '<link>'
HOME=$TMP/app target/release/flicks-desktop --native --trace pacing.jsonl
```

## FLK-50 — launch intro pacing (2026-10-30)

Apple M5 Max, macOS 27.2, release profile, code `ea0df52`, Rune `e450c92`. The real app
(`flicks-desktop --native --trace`, temp HOME paired with a local server) on an LG 5K external
display at **60 Hz** (the trace header's `hz`). The 120 Hz run on the MacBook (exonbook) is still to
do: it was unreachable again (no mDNS answer). Intervals are counted from the frame the intro's
beats start (the first animating `tracks` frame) to the end of the trace.

| Run | Beats start | Intervals | Present interval p50 / p95 / max (ms) | Missed (> 25 ms) | Encode CPU p50 / p95 / max (ms) |
|---|---:|---:|---:|---:|---:|
| 1 (`--frames 150`) | frame 5, 269 ms | 144 | 16.67 / 16.67 / 16.67 | 0 | 0.79 / 1.12 / 2.06 |
| 2 (`--frames 150`) | frame 6, 237 ms | 143 | 16.67 / 16.67 / 16.67 | 0 | 0.79 / 1.18 / 2.34 |
| 3 (whole intro) | frame 7, 259 ms | 156 | 16.67 / 16.67 / 16.68 | 0 | 0.81 / 1.17 / 2.55 |

- The intro runs on the display link and stops with it: in run 3 the last frame is drawn at
  2.86 s, the last animating one, and the process sits at 0.0% CPU after (`top`). (`--frames N`
  keeps a trace redrawing while idle, Rune `window/draw.rs`, so runs 1 and 2 don't show this.)
- A launch's first frames stall on its own work: 83 to 133 ms and 33 ms gaps in the first 0.3 s
  (window activation, the first catalog). The intro waits for three on-time frames in a row (at
  most 0.6 s) before starting its beats; without that wait the first frame's flight lost a 100 ms
  frame in every run.
- Two earlier runs of the same logic (with debug logging) each had one 33 ms miss mid-intro
  (at 116 and 600 ms into the beats, 1.1 to 1.5 ms of CPU, load average 12 to 14 from other
  agents' builds), so it's not the intro's own work.
- Sound sync (`flicks-brand/intro/intro.mp4`, rendered with Rune's `--video` and muxed with
  intro.wav at the beats' start): the middle and front frames reach full size at 0.465 s and
  0.578 s after the start, the sound's two hits at 0.46 s and 0.58 s (onset analysis of the
  muxed file).

### Reproduce

```sh
cargo build --release -p flicks-desktop
HOME=$tmp target/release/flicks-desktop --native --trace pacing.jsonl   # close the window after the intro
# Intervals from present_dt_ms after the first frame whose quality.why has "tracks"; CPU from
# quality.cpu_ms; missed = intervals > 1.5 refresh periods.
```

## FLK-6 review fixes — player lifetime, idle and controls (2026-10-30)

Release desktop on macOS, a 60 Hz display, temp server/client homes and the existing TV test
media read-only. The local server was stopped with SIGSTOP to block its reads, then resumed;
no other server or app process was stopped. The following are after-fix measurements, not
before/after CPU comparisons.

| Check | Result |
|---|---|
| Back during a blocked network open | Home visible, 0.0% CPU in 5 one-second `top` samples |
| Paused during playback | 0.0% CPU in 6 samples |
| Paused from a visible Buffering state | 0.0% CPU at rest in 4 samples; one earlier 0.7% sample while a progress save completed |
| Unpaused Buffering | 0.3% CPU from the playback thread's existing 4 ms polling; compositor no longer animates |
| Downloaded episode with server stopped | Resume works; offline 229,763 ms progress appears in Continue Watching |
| Reconnect by next desktop launch | Server receives exactly 229,763 ms, pending offline record cleared |
| Queued seeks | 40 queued arrow deltas become one seek; latest absolute target replaces older targets; Stop wins over seeks and track changes (desktop test) |

The real app also accepted 20 successive right-arrow presses and was idle immediately after
pause. A true held-key test remains unverified: the GUI driver's key calls serialized at about
one per second, and a separate PID-targeted autorepeat injection did not reach the window.
No claim of native held-key coverage is made from those attempts.

Native Rune trace (`--native --trace`, without `--frames`, which would keep an idle app drawing):
**1,749 player present intervals**, p50 **16.667 ms**, p95 **16.668 ms**, max **16.692 ms**,
**0 missed refreshes** (>25 ms). Window startup/intro are excluded; the interval starts with the
Play press and ends at the last paused-control frame. Each top/bottom panel had **80 intermediate
opacity samples** across its entrances/exits. One exit fell from 0.979 to 0.004 over 283 ms,
rather than unmounting at once. The existing Rune transition path handles reduced motion;
this native trace used normal motion. See [the real blocked-open screen](images/flk-6-slow-open.png).

### Fixed logo rasterization

Size-optimized release web build (`apps/web/build.sh`, wasm-bindgen 0.2.130), including the
merged intro and stills changes:

| Build | Shipped wasm bytes |
|---|---:|
| Main `8e027c9`, runtime SVGs | 7,506,703 |
| Fixed 3× PNG lockup and intro pieces, no Rune `svg` | 6,709,110 |
| Saved | **797,593 (10.6%)** |

Before the intro merge, the same cut measured 7,492,245 → 6,665,903 bytes (826,342 saved).
The lockfile drops 15 package entries, including the duplicate PNG version. The intro's boxes,
colors, springs and sound are unchanged; its fixed pieces also needed rasterizing after the
merge because `ImageCache::svg` otherwise returns an unsupported-feature image at runtime.

Verification: `scripts/check.sh` passed (format, locked workspace Clippy with warnings denied,
dependency policy, **819 tests/doctests passed, 36 ignored**). Includes 21 UI tests, 3 desktop
unit tests, the existing eight-scene desktop render test, and the client offline-progress
sync test against a real server. No private trace, playback screenshot, credentials, downloaded
media or runtime database is committed.
