# Flicks roadmap

The plan of work, split into cards that agents can take in parallel. Design: [`docs/architecture.md`](docs/architecture.md). Rules and workflow: [`AGENTS.md`](AGENTS.md).

Live status lives on the board (`board ls`), not in this file. Each card here has the same ID on the board. Only the coordinator edits this file: it adds, splits and retires cards. Agents note progress on the board.

Each card lists what it **owns** (the paths it may change), what it **needs** first, and when it's **done**. Two cards that own the same paths never run at once. Feature crates that the server mounts may also add their one mount line in `crates/server/src/routes.rs`; that's the only shared edit a card may make without saying so up front. Code to copy from FilmCraft comes from `https://github.com/storytold/filmcraft` at `fed0f42` (copied in with its notice, never depended on).

## Done on main (coordinator bootstrap)

Workspace `unsafe_code = "deny"` with `#![forbid(unsafe_code)]` in every crate; `deny.toml` licence policy; `scripts/check.sh` runs `cargo deny`; Deadbolt provenance in `THIRD_PARTY_NOTICES`; architecture reworked after review (auth, listeners, users, frame path, audio, subtitles, crate list).

## Phase 1: play a file, fast

Goal: a Rune window on macOS that plays a local MKV with hardware decode, correct audio and instant seeking, with measured numbers, while server foundations land beside it.

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-1 | **CI.** Workflow running `scripts/check.sh` and `cargo deny check` on macOS, Windows and Linux runners, with `FLICKS_REQUIRE_ORACLES=1` and an ffmpeg build that has every oracle encoder (libaom-av1, libkvazaar), so oracle tests can't skip | `.github/`, `scripts/` | — | CI green on all three; a GPL dependency or a missing oracle fails it |
| FLK-2 | **Media benchmarks.** Decode fps per codec (1080p and 4K, 8/10-bit), demux throughput, Cues seek latency over a simulated 30 ms link, open time and peak memory per container; baseline in `docs/perf.md` | `crates/bench/`, `docs/perf.md` | — | One command prints every number; baseline recorded |
| FLK-29 | **Container streaming fixes.** MKV: seek with `CuePosition::relative_position` instead of scanning clusters, `keyframe_index` from Cues without `build_index`, open options that skip attachments and tags, zlib content compression, `S_HDMV/PGS` codec; MP4: `dvh1`/`dvhe` with `dvcC`/`dvvC`, compact sample tables; MPEG-TS: open without reading the file, seek by PCR bisection; a prefetching, range-coalescing `ByteSource`; HEVC HDR SEI (137, 144) | `crates/container/`, `crates/mpegts/`, `crates/hevc/src/sei*` | — | Opening a 50 GB MKV reads < 1 MB; keyframe seek is one range read; peak memory for a 2 h MP4 < 5 MB; fuzz suites pass |
| FLK-3 | **Apple hardware decode.** `crates/platform`: VideoToolbox H.264 and HEVC 8/10-bit to `CVPixelBuffer`/IOSurface on macOS, iOS and tvOS; start from FilmCraft's `crates/platform` | `crates/platform/` | — | 4K HEVC 10-bit ≥ 120 fps on M1; output matches the software decoder on conformance clips; builds for `aarch64-apple-ios` and `aarch64-apple-tvos` |
| FLK-4 | **GPU frames.** `crates/gpu`: a Rune `Compositor` that draws video with Rune's device in Rune's pass: zero-copy IOSurface import, NV12/P010 to display colour (BT.601/709/2020), scaling, BT.2390 tone mapping to SDR; CPU frames for software decode | `crates/gpu/` | FLK-3 for import (software frames first) | 4K P010 frame drawn in < 1 ms on M1 with no intermediate texture; golden images match |
| FLK-30 | **Audio pipeline.** `crates/audio`: canonical channel layout from each decoder's order, BS.775 downmix, resampling, DRC off by default, device output with latency reporting | `crates/audio/` | — | 5.1 AAC, AC-3 and Opus all play centre in the centre and downmix correctly to stereo; resampler measured against a reference |
| FLK-5 | **Player core.** `crates/player`: demux, decode (hardware first, software fallback), audio master clock with device latency, video presentation against display timestamps, seek, buffering, dropped-frame stats, track selection | `crates/player/` | FLK-29, FLK-30 (FLK-3, FLK-4 as they land) | 2 h 4K MKV plays with 0 dropped frames and A/V offset < 20 ms; local keyframe seek < 100 ms |
| FLK-6 | **Desktop app, player screen.** `apps/desktop`: Rune window with the `gpu` compositor, controls, track menus, `flicks://` and file arguments | `apps/desktop/`, root `Cargo.toml` members line | FLK-4, FLK-5 | Opens a file from the command line and plays it with controls on macOS |
| FLK-7 | **Server identity, users and storage.** Strip password items from `core`; server device log with the server key as genesis; users and roles (admin, viewer, libraries); `store` server mode (OS-wrapped secrets, no Argon2id, small cache, Windows DACLs) | `crates/core/`, `crates/store/` | — | Membership tests pass with server logs; a reset admin signed by the server key wins removal races; service-style open uses < 8 MB and no passphrase |
| FLK-8 | **Library scanner.** `crates/library`: folders, folder-hash rescans, file watching, header probe through `container` (Cues index, codecs, HDR, Dolby Vision, audio), OSHash, SQLite catalog | `crates/library/` | FLK-29 | 10k-file library: first scan measured; unchanged rescan opens no media files |
| FLK-9 | **Rune: HDR target.** EDR / `Rgba16Float` swapchain and colour space in Rune, as a Rune PR | Rune repo only | — | PR open on Rune with its tests |

## Phase 2: server and client

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-10 | **Server API and auth.** `crates/server` and `crates/protocol`: direct listener (TLS keyed to the server key), ingress listener, signed requests with nonce cache, code-proof pairing, TV pending slots with confirmation codes, rate limits, folder denylist, `/pair` page, `flicks serve` / `flicks pair` / `flicks admin reset` | `crates/server/`, `crates/protocol/` | FLK-7 | A relayed pairing request can't be redeemed by another key; replayed and altered requests fail; revoked devices fail on the next request; fuzzed unauthenticated routes |
| FLK-11 | **Playback endpoints.** `crates/playback`: `/play` planner, byte-range serving, progress endpoints, mounted by `server` | `crates/playback/` | FLK-8, FLK-10 | One-request start; warm seek first byte < 60 ms at 30 ms RTT |
| FLK-38 | **Filename parser.** `crates/names`: titles, years, seasons, episodes, editions, versions from paths | `crates/names/` | — | ≥ 98% correct on a 2,000-path test corpus of real naming styles |
| FLK-12 | **Metadata.** `crates/metadata`: TMDB matching (within TMDB's cache terms), artwork cache, unmatched queue | `crates/metadata/` | FLK-8, FLK-38 | 95% of a test library matches automatically |
| FLK-13 | **Client core.** `crates/client`: server list, address racing, request signing, catalog merge across servers, progress merge, network byte source with prefetch, loopback player proxy | `crates/client/` | FLK-10 | Two servers, one merged catalog; progress converges; loopback proxy rejects cross-origin requests |
| FLK-14 | **Desktop app, library screens.** Home rows, detail, search, add server (QR link, `flicks://`, LAN discovery, typed address with confirmation code) | `apps/desktop/` | FLK-6, FLK-13 | Browse and play from two servers |
| FLK-15 | **Admin app.** `apps/admin`: folders, scan status, matches, users and devices, pairing QR and TV codes, listeners, relay status, settings | `apps/admin/`, root `Cargo.toml` members line | FLK-10 | Pair a device and add a folder without the CLI |
| FLK-16 | **Service install and discovery.** launchd, Windows service, systemd; DNS-SD `_flicks._tcp` | `crates/server/src/service*`, `crates/server/src/discovery*` | FLK-10 | Install, start, restart and uninstall pass on CI runners for all three OSes; reboot survival checked once by the coordinator |
| FLK-17 | **iroh transport.** Tuned endpoint (windows, BBR, parallel ranges), embedded `iroh-relay` option, n0 fallback warning | `crates/net/` | FLK-10 | ≥ 80% of iperf3 on a 1 GbE Linux LAN with 4 range streams; relay-to-direct time logged |
| FLK-31 | **More audio codecs.** `crates/flac`, `mp3`, `vorbis`, `alac`, `pcm` from their specifications | those crates | — | Conformance streams match the ffmpeg oracle; fuzz suites |
| FLK-22 | **Text subtitles.** `crates/subtitle`: SRT, WebVTT, ASS/SSA with MKV-attached fonts, shaped with a crates.io shaper into RGBA quads; no Rune dependency | `crates/subtitle/` | FLK-29 | Styled ASS renders at the right times; builds for iOS and Android targets |
| FLK-33 | **PGS subtitles.** Decode `S_HDMV/PGS` into the same quads | `crates/subtitle/src/pgs*` | FLK-22 | Blu-ray PGS tracks render at the right times and positions |

## Phase 3: every device and format

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-18 | **Jellyfin API.** `crates/jellyfin`: Jellyfin 12 subset, Quick Connect only, direct listener only by default | `crates/jellyfin/` | FLK-11 | Infuse, Swiftfin and jellyfin-web browse and direct play |
| FLK-19 | **Remux.** Stateless fMP4/CMAF writer (`write_init_segment`, `write_media_segment(seq, base_dts, …)`), DTS from PTS and reorder depth, `hvc1`, separate audio rendition; HLS routes | `crates/container/src/isobmff/cmaf*`, `crates/playback/src/hls*` | FLK-11, FLK-29 | Safari and AVPlayer play an MKV source with B-frames and seek |
| FLK-20 | **Windows and Linux hardware decode.** Media Foundation/D3D11 and VA-API, with zero-copy import in `gpu` | `crates/platform/src/{windows,linux}*`, `crates/gpu/src/import*` | FLK-3, FLK-4 | Same bar as FLK-3 on each OS |
| FLK-34 | **Android hardware decode.** MediaCodec to `AHardwareBuffer`, imported through Vulkan | `crates/platform/src/android*`, `crates/gpu/src/import*` | FLK-3, FLK-4 | 4K HEVC 10-bit plays on a Shield or Chromecast with Google TV |
| FLK-21 | **DTS.** `crates/dts`: DTS core (and the core of DTS-HD) | `crates/dts/` | FLK-30 | Conformance streams match the ffmpeg oracle |
| FLK-36 | **TrueHD.** `crates/truehd`, with Atmos metadata kept for passthrough | `crates/truehd/` | FLK-30 | Conformance streams match the ffmpeg oracle |
| FLK-37 | **Passthrough.** IEC 61937 framing of AC-3, E-AC-3, DTS and TrueHD to receivers | `crates/audio/src/passthrough*` | FLK-30 | Framing matches the oracle's S/PDIF muxer byte for byte; receiver check by the coordinator |
| FLK-32 | **Dolby Vision.** `crates/dovi`: RPU parsing, profile 7 EL strip to 8.1, profile 5 reshaping in `gpu` | `crates/dovi/`, `crates/gpu/src/dovi*` | FLK-4, FLK-29 | Profile 5, 7 and 8 samples display correct colour |
| FLK-35 | **H.264 High 10.** 10-bit plane path in `crates/h264` | `crates/h264/` | — | Hi10P conformance streams match the oracle |
| FLK-23 | **C ABI.** `crates/ffi` for mobile apps | `crates/ffi/` | FLK-13 | A C test program lists a server's catalog |
| FLK-24 | **Apple apps.** `apps/apple`: iOS, iPadOS, tvOS on the C ABI and player | `apps/apple/` | FLK-23, FLK-3 | Plays from a server on Apple TV |
| FLK-25 | **Android apps.** `apps/android`: phone and TV modules | `apps/android/` | FLK-23, FLK-34 | Plays from a server on Android TV |
| FLK-26 | **Transcode.** `crates/encode` and `crates/transcode`: hardware encode on all three OSes, FilmCraft's software H.264 encoder, transcode sessions | `crates/encode/`, `crates/transcode/` | FLK-19, FLK-20 | 4K HDR → 1080p SDR in real time on an M1 and an Intel iGPU |
| FLK-27 | **Analysis.** `crates/analysis`: trickplay, intro and credits markers, loudness in one pass per file | `crates/analysis/` | FLK-11 | Each file read once; markers served by the playback API (and Jellyfin media segments once FLK-18 is in) |
| FLK-28 | **Ten-foot mode.** Focus navigation, remotes and controllers in Rune, then the desktop app's TV layout | Rune repo, `apps/desktop/` | FLK-14 | Whole app usable with arrow keys only |

## Later

Profiles across servers, friends and sharing, offline downloads, watch together, Live TV and DVR, web client.
