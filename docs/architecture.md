# Flicks architecture

2026-10-30. Fourth draft, after the first plan review. No product code yet; `crates/core`, `crates/net` and `crates/store` are copied from Deadbolt (commit 7595d31), and the containers and decoders from FilmCraft and craft-libs (see `THIRD_PARTY_NOTICES`). Research: `research/2026-10-30-fast-stack.md` (its household mesh, jellyfin-ffmpeg and libmpv parts are superseded by this doc).

## Principles

1. **Everything lives in this repo.** Flicks never depends on another project's repo: no git dependencies on FilmCraft, Isocast, Deadbolt or anything else. Code we want from them is copied in, renamed, owned and maintained here. Copied code keeps its original copyright and licence notice in `THIRD_PARTY_NOTICES`.
2. **Rune is the one exception.** It's our UI framework, so Flicks depends on it (pinned rev, `default-features = false` so none of its media crates come along), builds every desktop UI in it, and contributes what's missing upstream instead of working around it.
3. **Foundation crates from crates.io are fine** (decided 2026-10-30): tokio, iroh, rustls, wgpu, redb, SQLite bindings, text shaping, compression and OS bindings. Media, playback, library and protocol code is ours.
4. **No FFmpeg, no libmpv, no GPL.** Demuxing, decoding, encoding, remuxing, subtitles and playback are our own Rust code, with the operating system's hardware decoders and encoders called directly. ffmpeg is only a test oracle.
5. **Insanely optimized.** Every hot path has a budget in `docs/perf.md` and a benchmark that fails CI when it regresses: idle memory, cold start, time to first frame, seek, unchanged rescan, decode fps, dropped frames. Zero-copy from hardware decoder to screen. No process per stream. Nothing runs when nothing changes.

## Shape

Flicks has two programs: a **server** that owns folders of video, and a **client** that connects to any number of servers and shows them as one Netflix-style library. Both run on macOS, Windows and Linux. Neither needs anything we host: no account, no flicks.dev, no relay of ours. A server is reached at whatever address the user already has.

```text
  Flicks Server (Mac)       Flicks Server (Windows)      Flicks Server (Linux NAS)
  ~/Movies, ~/TV            D:\Media                     /srv/media
  Rune admin window         Rune admin window            headless: `flicks serve` + CLI
        ▲                         ▲                              ▲
        │ LAN 192.168.1.20        │ tailnet 100.71.3.9           │ tunnel https://nas.example.com
        │                         │                              │ or iroh (no setup)
        └───────────────┬─────────┴──────────────────────────────┘
                        │ each server: its own key, own library, own users and devices
                 Flicks client (Rune)
                 one merged library, continue watching across all servers
```

- **Servers are independent.** Each has its own library, database, users and devices. Servers never talk to each other.
- **The client consolidates.** It connects to every server you've added, merges their catalogs by TMDB id (the same film on two servers is one tile with two sources), picks the best source to play, and merges watch progress.
- **Core and thin UIs.** The server is a background service (launchd, Windows service, systemd) with no window. The Rune admin window and the CLI are clients of its local API.

## Reaching a server

A server is identified by its **key** (Ed25519), never by its address. A server has two kinds of listener, and clients may know several addresses for each:

| Listener | Addresses | Transport | Treated as |
|---|---|---|---|
| **Direct** | LAN `https://192.168.1.20:7686`, tailnet `https://100.71.3.9:7686` | TLS 1.3 whose certificate key **is** the server key; clients accept exactly that key, so certificate renewal never breaks pinning | Local network or tailnet, by source address |
| **Ingress** | Tunnel or reverse proxy, `https://nas.example.com` (OpenTunnel, Cloudflare Tunnel, Caddy) | Plain HTTP on a separate loopback port or Unix socket that only the proxy talks to; the proxy terminates public TLS | Always remote, never local, whatever the TCP peer address |
| **iroh** | `iroh:<server key>` | QUIC authenticated to both keys end to end, hole punching | Remote |

- The ingress listener is the only place `X-Forwarded-For` or `CF-Connecting-IP` is read, and rate limits key on it there. Loopback traffic on the ingress port is never "local".
- The client tries every known address at once, keeps the first that proves the server key, and prefers direct LAN, then tailnet, then iroh, then ingress.
- iroh connects directly (hole punching) most of the time. When it can't, it falls back to a relay: a relay the user runs (embedded `iroh-relay` on a server with a reachable port, or a VPS), else n0's public relays, which are throttled; the admin window warns when streams are relayed through n0. Address publishing to n0's DNS is on only for servers with iroh enabled, never for clients.
- Nothing in the protocol names flicks.dev, which is only the website.

## Authentication

Modelled on OpenCode's `opencode pair` (one-time code, QR with every reachable address, try them all), but with a key per device and a pairing code that never travels over the wire, so the server can face the internet through a tunnel.

### Server identity and storage

- The **server key** is the genesis member of the server's device log (Deadbolt's membership, so it's the most senior device and wins every removal race).
- Server secrets are stored without a passphrase so the service starts unattended: wrapped by the OS (Keychain, DPAPI with a DACL limited to Administrators and the service SID, a 0600 file or the Secret Service on Linux). `store` gets a server mode with no Argon2id and a small cache; Argon2id stays for clients that opt into a passphrase.
- Device keys on clients live in the OS keychain where there is one.

### Pairing from a QR or link

1. The admin opens a pairing window (admin app or `flicks pair`). It shows a QR and a link:
   - QR: `{"v":1,"server":"<server key>","code":"<128-bit one-time code>","urls":["https://192.168.1.20:7686","https://nas.example.com","iroh:<key>"],"user":"<user id or new>","role":"viewer"}`
   - Link: `https://nas.example.com/pair#<code>.<server key>`. The fragment never reaches the proxy. The server serves a static, no-referrer `/pair` page that never sends the fragment anywhere; it offers **Open in Flicks** (`flicks://pair#…`) and **Copy code**.
2. The client derives `k = HKDF(code, "flicks pair v1" ‖ server key)` and sends to every address in parallel: its device key, name and nonce, plus `HMAC(k, transcript)`. **The code itself is never sent**, so a tunnel or MITM that sees the request can't redeem it for another key.
3. The server checks the HMAC against its open codes in constant time, records the device (signed `Add`, with the user and role from the window), and answers with its server-key signature over the transcript. The client checks that signature against the server key from the QR.
4. Redeeming is idempotent for the same device key within the window, so parallel attempts at several addresses can't burn the code. Any other device key gets "used".

### Pairing a TV or anything that can't scan

One flow on and off the LAN: the TV finds the server through DNS-SD (`_flicks._tcp`) or the user types its address. The TV sends its device key and gets a pending slot (at most 5 open, 10 minutes each), and shows an 8-character code. The admin types the code in the admin app; both screens then show the same 6-digit confirmation derived from the TV key, server key and code, and the admin approves only if they match. That confirmation catches a MITM, since the TV didn't know the server key beforehand.

### Every request

- **Signed, not bearer.** Every request carries an Ed25519 signature by the device key over method, path, body hash, timestamp and a nonce, checked against a per-device nonce cache (±60 s). A tunnel can read and relay traffic, but can't forge, alter or replay requests, so there are no session tokens to steal. Signing and verifying cost microseconds.
- **Live checks.** Every request is checked against the in-memory device list, so removing a device takes effect on its next request; iroh connections from it close at once (Deadbolt's membership gate).
- **Admin routes** (folders, devices, settings) are refused on the ingress listener unless the admin turns that on, since the tunnel operator can read the responses. Adding a folder refuses system and secret locations (`/etc`, `~/.ssh`, the Flicks data dir, and their Windows equivalents) after canonicalizing the path.
- **Players never see credentials.** The client core fetches from the server (signing each range request) and serves the platform player from loopback, on a random port, at paths with a per-session random capability, rejecting any request with a browser `Origin` or `Sec-Fetch-Site: cross-site`.
- **Only three routes are unauthenticated:** `info` (server key and protocol version, no name or library), pairing redemption and the TV pending-slot request. All are rate-limited per client IP and globally.
- **Local admin and recovery.** The admin app on the server machine authenticates with a local credential file readable only by that account (0600 or a DACL). `flicks admin reset` on the server machine signs a new admin's `Add` with the server key.
- **Confidentiality through a tunnel:** the tunnel operator can read responses (catalogs, media). Use the direct listener or iroh when that matters.

## Users and watch progress

- **Server users.** A server has users (people), and each paired device belongs to one, chosen in the pairing window ("Who is this for?"). Your Mac and your Apple TV paired as you share Continue Watching on that server from day one.
- **Progress** is stored per (user, item) on every server that holds the item, as a last-writer-wins value ordered by a hybrid logical clock. The client writes on start, pause, stop, a big seek, finishing, and every few minutes while playing. When two servers disagree, the client takes the newest and writes it back to the other.
- **Profiles across servers (later).** A profile is a Deadbolt-style signed device log for one person, so a new TV paired into your profile reaches all your servers without pairing each one. Servers would grant profiles instead of single devices; a server user with one device is the degenerate case, so the wire format doesn't change.

## Server

- **Folders:** local folders or mounted shares. The scanner walks them with folder hashes (an unchanged folder costs one directory listing), watches local disks, reads headers with `container` (keyframe index from Cues without scanning clusters, codec data, HDR and Dolby Vision, audio formats), identifies files by OSHash, parses names with our own parser, and matches on TMDB.
- **Admin window (Rune):** folders and scan status, matches, users and devices (pair, rename, role, remove), libraries per user, transcoding, addresses and listeners, relay status, logs.
- **Playback:** one request (`/play/{item}?t=resume` with the client's capabilities) answers with:
  1. **Direct play:** byte ranges of the original file. No CPU. Every Flicks app takes this path for everything our player supports.
  2. **Remux:** fMP4/CMAF HLS segments cut from the keyframe index in process, for AVPlayer, browsers and TVs. Video in `hvc1` with parameter sets, monotonic DTS derived from PTS and the stream's reorder depth (MKV stores PTS only), audio as a separate rendition.
  3. **Transcode:** our own pipeline in process: hardware decode, scale before tone mapping on the GPU, hardware encode (VideoToolbox, Media Foundation with NVENC/QSV/AMF, VA-API) or our software H.264 encoder. Audio-only transcodes serve third-party clients that lack a codec we can decode (AC-3, E-AC-3, Opus, FLAC to AAC for browsers).
- **Background analysis:** one pass per file for trickplay, intro and credits markers, and loudness, keyed by content hash.
- **LAN discovery:** `_flicks._tcp` through the OS's DNS-SD responder (Bonjour, Avahi, Windows DNS-SD).

The `server` crate is plumbing: listeners, auth, routing, service install. Each feature crate (`library`, `metadata`, `playback`, `transcode`, `jellyfin`, `analysis`) exposes its routes and services behind a small interface that `server` mounts; mounting is a one-line change in `server`. Request and response types shared with clients live in `protocol`.

## Client

- **Library:** home screen merged across servers (Continue Watching, Next Up, Recently Added, libraries, genres), detail pages, search, versions and sources. Offline servers grey their titles out.
- **Add server:** scan a QR, open a `flicks://` link, paste a link, or find a server on the LAN or type an address and pair as a TV does.
- **Client core:** `client` holds the server list, address racing, request signing, catalog and progress merge, and the network byte source for the player. The Rune desktop app uses it directly; the Apple and Android apps through `ffi`, our own C ABI.

## Media stack

All ours, in this repo. FilmCraft's pure-Rust media code (checked frame by frame against ffmpeg) is copied in; its hardware, GPU and encoder code (`storytold/filmcraft` at fed0f42: `crates/platform`, `crates/gpu`, `crates/h264enc`) is the starting point for those cards, copied in by the card that needs it.

| Area | Crates | Have | Need (card) |
|---|---|---|---|
| Containers | `container`, `mpegts` | MKV/WebM with Cues, MP4/MOV (fragmented), MPEG-TS | Seek from Cues positions without cluster scans, lazy open, compact MP4 sample tables, MPEG-TS seek by PCR bisection, prefetching byte source; zlib content compression and PGS in MKV; `dvh1`/`dvhe` and `dvcC`/`dvvC` in MP4; stateless fMP4 segment writer (FLK-29, FLK-19) |
| Video | `h264`, `hevc`, `av1`, `vp9` | H.264 8-bit (500–600 fps 1080p), HEVC Main/Main 10 (~225 fps 1080p), AV1, VP9 | H.264 High 10 (FLK-35); HDR SEI (mastering display, content light level) (FLK-29) |
| Dolby Vision | `dovi` | — | RPU parsing, profile 7 enhancement-layer strip to 8.1, profile 5 reshaping on the GPU (FLK-32) |
| Audio | `aac`, `ac3`, `opus`; `flac`, `mp3`, `vorbis`, `alac`, `pcm`; `dts`, `truehd` | AAC, AC-3, E-AC-3, Opus | FLAC, MP3, Vorbis, ALAC, LPCM (FLK-31); DTS core and TrueHD (FLK-21, FLK-36); passthrough to receivers (FLK-37) |
| Hardware decode | `platform` | — | VideoToolbox on macOS, iOS and tvOS (FLK-3); Media Foundation and VA-API (FLK-20); MediaCodec on Android (FLK-34). `unsafe` lives here |
| GPU | `gpu` | — | Zero-copy import of hardware frames into wgpu textures, YUV (NV12, P010) to display colour, scaling, BT.2390 tone mapping, Dolby Vision reshaping (FLK-4) |
| Encode | `encode` | — | Hardware encoders and FilmCraft's software H.264 encoder, for server transcodes (FLK-26) |
| Subtitles | `subtitle` | — | SRT, WebVTT, ASS/SSA with fonts from MKV attachments (FLK-22); PGS (FLK-33). Portable: shapes text with a crates.io shaper and produces RGBA quads for `gpu`, with no Rune dependency, so it builds for iOS, tvOS and Android |

### Player

`player` puts it together and builds for macOS, Windows, Linux, iOS, tvOS and Android:

- **Video:** hardware decode first, our software decoders when hardware can't (Hi10P, odd profiles); frames stay on the GPU.
- **Audio** (`audio` crate, FLK-30): decode, map every decoder's channel order to one canonical layout, downmix (ITU-R BS.775) when the device has fewer channels, resample to the device rate, dynamic range compression off by default, device output.
- **Clock:** audio is the master clock, including the output device's latency; video frames present against display timestamps; frame-rate matching where the OS allows.
- **Tracks:** the player picks the best track it can decode; until DTS and TrueHD land, a file with only those tracks reports that clearly instead of playing silently.

### Drawing frames in Rune

Decided 2026-10-30: the desktop app attaches a Rune `Compositor` (`App::compositor`, available on every platform Rune supports), so `gpu` draws video with **Rune's own wgpu device** into Rune's frame target, in the same pass, before Rune draws the UI over it. Hardware frames are imported straight into textures on that device; there is no intermediate RGBA copy. What Rune needs (FLK-9):

1. An HDR-capable target (EDR / `Rgba16Float` swapchain and colour space) so HDR video can reach HDR displays; until then HDR is tone mapped to SDR.
2. Optionally later, a first-class `video` element built on `player`, offered upstream as the cross-platform backend for Rune's AVPlayer-only `Player`.

## Apps and platforms

| Platform | App | Plays with |
|---|---|---|
| macOS, Windows, Linux | Flicks (Rune) | Our player |
| Server admin, all three | Flicks Server window (Rune) or CLI | — |
| iOS, iPadOS, tvOS | Flicks (SwiftUI shell + client core) | Our player; the system player only where Apple requires it (AirPlay, Picture in Picture) |
| Android, Android TV, Fire TV | Flicks (Compose shell + client core) | Our player, with MediaCodec hardware decode |
| Anything else (Roku, LG, Samsung, Kodi, browsers) and until our apps ship | Infuse, Swiftfin, Streamyfin, Findroid, Jellyfin apps, jellyfin-web | Through the Jellyfin-compatible API |

### Jellyfin-compatible API

A subset of the Jellyfin 12 API (about 30–50 operations: auth, views, items, resume, Next Up, PlaybackInfo, streams and HLS, images, sessions and progress, media segments), written from Jellyfin's OpenAPI spec, for the one server a Jellyfin client points at.

Sign-in is **Quick Connect** only: the app shows a code, and a user approves it in the Flicks admin or desktop app, choosing which user it belongs to. Jellyfin clients use bearer tokens, which is weaker than our signed requests, so those tokens are viewer only, listed and revocable per device, and accepted only on the direct listener (LAN and tailnet) unless the admin enables them on ingress.

## Crates

| Crate | Owns | Status |
|---|---|---|
| `core` | Device keys, signed entries, membership and roles, users | From Deadbolt; becomes server device logs and users (FLK-7) |
| `store` | redb storage for keys and logs | From Deadbolt; gains server mode (FLK-7) |
| `net` | iroh endpoint, membership gate, Deadbolt pairing and sync | From Deadbolt; tuned in FLK-17 |
| `container`, `mpegts`, `h264`, `hevc`, `av1`, `vp9`, `aac`, `ac3`, `opus`, `testkit` | Containers, decoders, test oracles | From FilmCraft and craft-libs |
| `flac`, `mp3`, `vorbis`, `alac`, `pcm`, `dts`, `truehd`, `dovi` | Remaining codecs and Dolby Vision | New |
| `platform`, `gpu`, `encode`, `subtitle`, `audio` | Hardware, GPU, encoders, subtitles, audio output | New |
| `player` | Playback engine | New |
| `bench` | Benchmarks behind `docs/perf.md` | New |
| `protocol` | Types shared by server and clients | New (FLK-10) |
| `library`, `names`, `metadata`, `playback`, `transcode`, `analysis`, `jellyfin` | Server features (`names` is the filename parser) | New |
| `server` | Listeners, auth, routing, service install, `flicks` CLI | New |
| `client`, `ffi` | Client core and its C ABI | New |
| `apps/desktop`, `apps/admin` | Rune apps | New |

`unsafe` is denied workspace-wide; every crate without it also carries `#![forbid(unsafe_code)]`. Only `platform`, `gpu` and `ffi` allow it, on their FFI modules, with a `// SAFETY:` comment per block.

## Rune contributions

1. **HDR target** (EDR / `Rgba16Float`) for HDR video output (FLK-9).
2. **`flicks://` URLs on Windows and Linux** (`on_open_url` is macOS-only).
3. **Ten-foot UI:** focus navigation by arrow keys, remotes and game controllers, overscan-safe layout (FLK-28).
4. **A cross-platform `Player` backend** from `player`, once it's solid.

tvOS and Android TV aren't on Rune's platform list, so those apps are native shells around the client core and player.

## Open questions

1. Ports: is 7686 free of common conflicts, and should the server try UPnP for a public port?
2. Live TV and DVR: later?
