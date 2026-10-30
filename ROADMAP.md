# Flicks roadmap

One platform at a time, simplest working version first. Design: [`docs/architecture.md`](docs/architecture.md). Rules and workflow: [`AGENTS.md`](AGENTS.md).

Live status is on the board (`board ls`); each card has the same ID there. Only the coordinator edits this file. Only the current milestone's cards are in `todo`; later ones wait in the backlog.

Each card lists what it **owns** (paths it may change), what it **needs** first, and when it's **done**. Cards that own the same paths never run at once. A card may add a dependency to a crate it doesn't own (`Cargo.toml` only) and says so in its final note.

## Milestone 1: macOS server

Done when: on a Mac, `flicks serve` scans a folder of movies and shows, matches them on TMDB, and another machine pairs with it from the CLI (`flicks connect <link>`, `flicks ls`, `flicks get <file> --range`). Agents prove this with automated tests on localhost and through a local TLS-terminating proxy; the coordinator checks it once with two Macs over LAN and a real tunnel.

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-1 | **CI.** GitHub Actions on macOS and Linux running `scripts/check.sh` with `FLICKS_REQUIRE_ORACLES=1` and a stock ffmpeg; oracle tests require only encoders every stock ffmpeg has (libx264, libx265, libvpx, libsvtav1) and skip seeds that need optional ones (libkvazaar, libaom); AV1 conformance vectors cached in CI instead of downloaded per run | `.github/`, `scripts/`, `crates/testkit/`, oracle and fuzz test files that name optional encoders or download vectors | — | CI green on both; a GPL dependency or a missing required oracle fails it |
| FLK-29 | **Fast container open.** MKV: open without scanning clusters, keyframe index from Cues (`keyframe_index` without `build_index`), seek with `CuePosition::relative_position`; MP4: compact sample tables | `crates/container/` | — | Opening a 50 GB MKV reads < 1 MB; keyframe index from Cues in < 5 ms; a 2 h MP4 opens in < 5 MB of memory; fuzz suites pass |
| FLK-12 | **Names and TMDB.** `crates/metadata`: parse title, year, season, episode and edition from paths; match on TMDB when `FLICKS_TMDB_KEY` is set; cache artwork as WebP | `crates/metadata/` | — | ≥ 98% correct on a 2,000-path corpus; ≥ 95% of a 200-title library matches, tested against recorded TMDB responses; works with no key |
| FLK-10 | **Server core.** `crates/server`: `flicks` binary, server key and in-memory TLS certificate, the SQLite database (migrations, single writer task) with users and devices, pairing window and redemption, signed requests, `/info` probe, folder add with denylist, CLI talking to the running server (`serve`, `folder`, `user`, `pair`, `device`, `status`), global rate limit. Leaves empty `api`, `cli_client` and `service` modules wired into the router and CLI for later cards | `crates/server/` (except `src/api*`, `src/cli_client*`, `src/service*`) | — | Automated tests on localhost and through a local TLS-terminating proxy: pairing works; a relayed pairing request can't be redeemed by another key; replayed, altered and removed-device requests fail; unauthenticated routes fuzzed |
| FLK-8 | **Scanner and catalog.** `crates/library`: folder-hash walk, FSEvents watching, probe through `container`, OSHash, catalog tables and queries on a SQLite connection it's handed (no database of its own), matching through `metadata` | `crates/library/` | FLK-29, FLK-12 | 10k-file library: first scan time recorded; unchanged rescan opens no media files; a new file appears within 2 s |
| FLK-11 | **Server API.** Catalog endpoints (home rows, libraries, items, search), `/play`, signed byte-range serving, progress | `crates/server/src/api*` | FLK-8, FLK-10 | Range read first byte < 5 ms on LAN; one request to start playback; progress round-trips |
| FLK-13 | **Client core and CLI.** `crates/client`: server list, `client.key`, request signing, address racing with the `/info` probe, catalog fetch; `flicks connect`, `flicks ls`, `flicks get` | `crates/client/`, `crates/server/src/cli_client*` | FLK-10, FLK-11 | Automated test: a client on localhost and one through a local TLS-terminating proxy pair, list the catalog and read a byte range |
| FLK-16 | **Runs at login on macOS.** `flicks service install|uninstall` (launchd) | `crates/server/src/service*` | FLK-10 | `launchctl bootstrap` starts it, `kickstart -k` restarts it, uninstall leaves nothing behind (agents never log the Mac out; the coordinator checks a real login once) |

## Milestone 2: macOS app

Done when: the Rune app on a Mac adds that server, shows its library, and plays any H.264/HEVC/AV1 MKV or MP4 in it with sound, seeking and SRT subtitles; progress shows up in Continue Watching.

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-3 | **macOS hardware decode.** `crates/platform`: VideoToolbox H.264 and HEVC 8/10-bit to `CVPixelBuffer` (start from FilmCraft's `crates/platform` at fed0f42) | `crates/platform/` | — | 4K HEVC 10-bit ≥ 120 fps on M1; matches the software decoder on conformance clips |
| FLK-4 | **Video renderer.** `crates/gpu`: plain wgpu (`~30.0.1`), NV12/P010 to display colour, scaling, HDR tone-mapped to SDR, `srgb_blend` aware; CPU frames and IOSurface import | `crates/gpu/` | — | Golden images match; 4K P010 draw time recorded (about 1 ms on an M5 Max, accepted as well under a 60 fps frame) |
| FLK-30 | **Audio.** `crates/audio`: canonical channel order, stereo downmix, resample, DRC off, CoreAudio output | `crates/audio/` | — | 5.1 AAC, AC-3 and Opus play centre in the centre and downmix correctly to stereo |
| FLK-9 | **Rune: no media crates through rune-kit.** `default-features = false` on `rune` in `rune-kit` and the theme crates, as a Rune PR | Rune repo only | — | PR open; a crate depending on `rune-kit` passes our `deny.toml` |
| FLK-5 | **Player.** `crates/player`: local file and network byte sources with prefetch, demux, hardware decode with software fallback, audio master clock, seek, buffering, track selection, dropped-frame count | `crates/player/` | FLK-3, FLK-4, FLK-30 | 2 h 4K MKV plays with 0 dropped frames and A/V offset < 20 ms; seek < 100 ms locally, < 300 ms from the LAN server |
| FLK-22 | **SRT and WebVTT subtitles.** `crates/subtitle` | `crates/subtitle/` | — | Cues show at the right times; fuzzed parsers |
| FLK-6 | **App: player screen.** `apps/desktop`: Rune window, video through `Compositor` and `El::effect`, controls, audio and subtitle menus; open a file or a server item | `apps/desktop/`, root `Cargo.toml` members line | FLK-5, FLK-9 | Plays a local file and a server item with controls |
| FLK-14 | **App: library.** Home rows, detail, search, Add Server, progress | `apps/desktop/src/library*` | FLK-6, FLK-11, FLK-13 | Browse and play from the Mac server; Continue Watching updates |
| FLK-15 | **App: Server screens.** For admins: folders, users, devices, pairing QR | `apps/desktop/src/server*` | FLK-14 | Pair a new device and add a folder without the CLI |

## Milestone 3: Linux server

Done when: the same server runs on a Linux box as a systemd service, and the Mac app shows its library merged with the Mac server's.

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-39 | **systemd service and inotify watching** | `crates/server/src/service*`, `crates/library/src/watch*` | M2 | Survives reboot on Linux; new files appear within 2 s |
| FLK-40 | **Two servers, one library.** Merge by TMDB id with per-source choice; offline servers grey out | `crates/client/src/merge*`, `apps/desktop/src/library*` | M2 | A film on both servers is one tile; unplugging one keeps its titles visible and greyed |

## Milestone 4: Linux app

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-20 | **VA-API decode** and DMA-BUF import into `gpu` | `crates/platform/src/linux*`, `crates/gpu/src/import*` | M3 | 4K HEVC 10-bit plays smoothly on an Intel iGPU |
| FLK-41 | **Linux audio output and app packaging** | `crates/audio/src/linux*`, packaging | M3 | The app installs and plays on Ubuntu and Fedora |

## Added 2026-10-30 (Ryan)

| ID | Card | Owns | Needs | Done when |
|---|---|---|---|---|
| FLK-45 | **Downloads.** Download a title's best source to the device (resumable byte ranges, size and OSHash checked), play the local copy when present (offline too), progress syncs when back online; a Downloads screen | `crates/client/src/download*`, `apps/desktop/src/downloads*` | FLK-5 for playback of the local copy | A downloaded episode plays with the network off and its progress reaches the server later |
| FLK-46 | **Share downloads nearby.** A device's downloads become a source other devices of the same user can play from on the LAN (the app runs a small Flicks server over its downloads folder) | later design | FLK-45 | A second Mac plays a title from the first Mac's download with the real server offline |
| FLK-47 | **Adaptive streaming.** The server offers a quality ladder (direct, then transcoded renditions) as HLS/CMAF; the player measures throughput, prefetches ahead and switches quality at segment boundaries | `crates/player/src/abr*`, server HLS routes | FLK-19, FLK-26 | On a throttled link (e.g. 8 Mbps dropping to 3 and back) playback doesn't stall and returns to full quality |
| FLK-48 | **App look: Netflix x Apple TV.** Big hero with backdrop and logo at the top (Apple TV top shelf), Netflix-style rows, focus and hover scale, blurred backdrops behind detail pages, smooth Rune motion; one look for every client | `apps/desktop/src/style*` and views | FLK-6 | Ryan approves screenshots |
| FLK-49 | **Web app.** The Rune app compiled to wasm (Rune's web host), served by the Flicks server at `/` so the API is same-origin, signing with WebCrypto P-256; browse, detail, search; reached through any HTTPS front with a real certificate (tailnet, tunnel) | `apps/web/`, a static route in `crates/server` | FLK-14 | Ryan opens it on his phone over the tailnet and browses the library |
| FLK-50 | **App intro.** A short Netflix-style intro on cold launch: the stacked-frames logo animates in (Rune springs, 120 Hz) with its own sound effect, about 2 s, skippable with any key or click, silent and reduced to a fade when reduced motion is on; the same on the web | `crates/ui/src/intro*`, `assets/brand/intro*`, the audio hook in apps/desktop and apps/web | FLK-48 (logo in the app) | Ryan approves a recording with sound |
| FLK-51 | **Artwork from the video.** When a title has no TMDB artwork, the server makes its own: a backdrop and episode stills from a representative frame of the file (software or hardware decode, skipping black and title frames), encoded once as JPEG into the server's data dir and served by the existing artwork route, so clients need no change | `crates/server/src/stills*` (new), the scanner hook | FLK-8, decoders | Home's hero and episode rows show real frames from the test episodes with no TMDB key; a first scan of the two episodes adds under ~1 s of stills work each |

Order: FLK-19 (HLS/CMAF writer) now, since adaptive streaming, AVPlayer and web playback all need it; FLK-26 (transcode) after it; then FLK-47.

## Milestone 5: Windows

Server (Windows service, ReadDirectoryChangesW, owner-only ACLs) and app (Media Foundation decode with D3D11 import, WASAPI output). Cards written when M4 is done.

## Milestone 6: Web

A browser client. Design and cards written when M5 is done.

## After that

Apple TV, iPhone and iPad; Android and Android TV; Jellyfin-compatible API; HLS remux and transcoding; Dolby Vision; HDR output; DTS, TrueHD, passthrough and more audio codecs; PGS and ASS subtitles; skip intro and trickplay; iroh connections and profiles across servers; LAN discovery; offline downloads; Live TV. These stay in the board backlog as reminders, not plans.
