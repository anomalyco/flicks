# Flicks roadmap

The plan of work, split into cards that agents can take in parallel. Design: [`docs/architecture.md`](docs/architecture.md). Rules and workflow: [`AGENTS.md`](AGENTS.md).

Live status lives on the board (`board ls`), not in this file. Each card here has the same ID on the board. Only the coordinator edits this file: it adds, splits and retires cards. Agents note progress on the board.

Each card lists what it **owns** (the paths it may change), what it **needs** first, and when it's **done**. Two cards that own the same paths never run at once.

## Phase 1: play a file, fast

Goal: a Rune window on macOS that plays a local MKV with hardware decode, audio and seeking, with measured numbers, while the server and client foundations land beside it.

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-1 | **Repo gates.** `cargo deny` with a permissive-licence allow-list, added to `scripts/check.sh`; a CI workflow running it on macOS, Windows and Linux | `scripts/`, `deny.toml`, `.github/` | — | A GPL dependency fails the check; CI is green on all three OSes |
| FLK-2 | **Media benchmarks.** Decode fps per codec (1080p and 4K, 8- and 10-bit), demux throughput, keyframe seek latency from Cues, peak memory; numbers in `docs/perf.md` | `crates/bench/`, `docs/perf.md` | — | One command prints every number; baseline recorded |
| FLK-3 | **macOS hardware decode.** `crates/platform`: VideoToolbox H.264 and HEVC (8/10-bit) to `CVPixelBuffer`/IOSurface. `unsafe` only here | `crates/platform/` | — | 4K HEVC 10-bit decodes at ≥ 120 fps; output matches the software decoder on conformance clips |
| FLK-4 | **GPU frames.** `crates/gpu`: NV12/P010 → RGB in wgpu, BT.601/709/2020, scaling, HDR → SDR tone mapping (BT.2390), zero-copy IOSurface import | `crates/gpu/` | — | Converts a 4K P010 frame in < 1 ms on M-series; golden images match |
| FLK-5 | **Player core.** `crates/player`: demux thread, decode workers (hardware first, software fallback), audio out, A/V clock, seek, buffering, dropped-frame stats; source trait for files and byte ranges | `crates/player/` | FLK-3, FLK-4 (stub until they land) | Plays 2 h MKV with 0 dropped frames; seek to keyframe < 100 ms from local disk |
| FLK-6 | **Desktop app, player screen.** `apps/desktop`: Rune window, video element, controls, track menus | `apps/desktop/` | FLK-5 | Opens a file from the command line and plays it with controls |
| FLK-7 | **Core as profiles.** Strip password-manager items from `core`; vault → profile; server device list with roles (admin, viewer, libraries) | `crates/core/`, `crates/store/` | — | Membership, pairing and sync tests pass with profile entries only |
| FLK-8 | **Library scanner.** `crates/library`: folders, folder-hash rescans, file watching, header probe through `container` (keyframe index, codecs, HDR, audio), OSHash, filename parser, SQLite catalog | `crates/library/` | — | 10k-file library: first scan measured; unchanged rescan opens no media files |
| FLK-9 | **Rune: frames on Windows and Linux.** Zero-copy wgpu texture hand-off for `NativeSurface` (D3D11/12, DMA-BUF/VA-API), as a Rune PR | Rune repo only | — | PR open on Rune with its tests |

## Phase 2: server and client

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-10 | **Server API and auth.** `crates/server`: HTTPS with key-derived self-signed cert, device-key challenge, session tokens, pairing windows (QR payload, TV codes), rate limits, `flicks serve` / `flicks pair` | `crates/server/` | FLK-7 | Pair a client, revoke it, and every request after revocation fails; fuzzed auth routes |
| FLK-11 | **Playback endpoints.** `/play` planner, byte-range serving, progress endpoints | `crates/server/src/play*` | FLK-8, FLK-10 | One-request start; warm seek first byte < 60 ms at 30 ms RTT |
| FLK-12 | **Metadata.** TMDB matching, artwork cache, unmatched queue | `crates/metadata/` | FLK-8 | 95% of a test library matches automatically |
| FLK-13 | **Client core.** `crates/client`: server list, address racing, auth, catalog merge across servers, progress merge, network byte-range source for the player | `crates/client/` | FLK-10 | Two servers, one merged catalog; progress converges across them |
| FLK-14 | **Desktop app, library screens.** Home rows, detail, search, add server (link, address + code, LAN discovery) | `apps/desktop/` | FLK-6, FLK-13 | Browse and play from two servers |
| FLK-15 | **Admin app.** `apps/admin`: folders, scan status, matches, devices, pairing QR, addresses, settings | `apps/admin/` | FLK-10 | Pair a device and add a folder without the CLI |
| FLK-16 | **Service install and discovery.** launchd, Windows service, systemd; DNS-SD `_flicks._tcp` | `crates/server/src/service*`, `crates/server/src/discovery*` | FLK-10 | Installs and survives reboot on all three OSes |
| FLK-17 | **iroh transport.** Tuned endpoint (windows, BBR, parallel ranges) as a server address | `crates/net/` | FLK-10 | ≥ 80% of iperf3 on a Linux LAN |

## Phase 3: every device

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-18 | **Jellyfin API.** `crates/jellyfin`: Jellyfin 12 subset, Quick Connect only | `crates/jellyfin/` | FLK-11 | Infuse, Swiftfin and jellyfin-web browse and direct play |
| FLK-19 | **Remux.** fMP4/CMAF HLS from the keyframe index, in process | `crates/container/src/cmaf*`, `crates/server/src/hls*` | FLK-11 | Safari and AVPlayer play an MKV source |
| FLK-20 | **Windows and Linux hardware decode.** Media Foundation/D3D11, VA-API | `crates/platform/` | FLK-3 | Same bar as FLK-3 on each OS |
| FLK-21 | **Audio gaps.** DTS core, TrueHD, Atmos metadata, passthrough to receivers | `crates/dts/`, `crates/truehd/`, `crates/player/src/audio*` | FLK-5 | Conformance streams decode; passthrough verified on a receiver |
| FLK-22 | **Subtitles.** `crates/subtitle`: SRT, WebVTT, PGS, ASS/SSA layout drawn with Rune text | `crates/subtitle/` | FLK-5 | Styled ASS and PGS render at the right times |
| FLK-23 | **C ABI.** `crates/ffi` for mobile apps | `crates/ffi/` | FLK-13 | A C test program lists a server's catalog |
| FLK-24 | **Apple apps.** `apps/apple`: iOS, iPadOS, tvOS targets on the C ABI and player | `apps/apple/` | FLK-23 | Plays from a server on Apple TV |
| FLK-25 | **Android apps.** `apps/android`: phone and TV modules | `apps/android/` | FLK-23 | Plays from a server on Android TV |
| FLK-26 | **Transcode.** Hardware encode on all three OSes, software H.264 fallback, transcode sessions | `crates/encode/`, `crates/platform/src/encode*`, `crates/server/src/transcode*` | FLK-19, FLK-20 | 4K HDR → 1080p SDR in real time on an M1 and an Intel iGPU |
| FLK-27 | **Analysis.** Trickplay, intro and credits markers, loudness, in one pass per file | `crates/analysis/` | FLK-8 | Each file read once; markers in Jellyfin clients |
| FLK-28 | **Ten-foot mode.** Focus navigation, remotes and controllers in Rune, then the desktop app's TV layout | Rune repo, `apps/desktop/` | FLK-14 | Whole app usable with arrow keys only |

## Later

Profiles across devices, friends and sharing, offline downloads, watch together, Live TV and DVR, web client.
