# Flicks architecture

2026-10-30. Third draft. No product code yet; `crates/core`, `crates/net` and `crates/store` are copied from Deadbolt (`../deadbolt`, commit 7595d31) and renamed. Research: `research/2026-10-30-fast-stack.md`.

## Principles

1. **Everything lives in this repo.** Flicks never depends on another project's repo: no git dependencies on FilmCraft, Isocast, Deadbolt or anything else. Code we want from them is copied in, renamed, owned and maintained here, as `core`, `net` and `store` were. Copied code keeps its original copyright and licence notice in `THIRD_PARTY_NOTICES` (FilmCraft is MIT or Apache-2.0, which requires that).
2. **Rune is the one exception.** It's our UI framework, so Flicks depends on it, builds every UI in it, and contributes what's missing upstream instead of working around it.
3. **Foundation crates from crates.io are fine** (decided 2026-10-30): tokio, iroh, rustls, wgpu, redb and OS bindings stay normal dependencies. Media, playback, library and protocol code is ours.
4. **No FFmpeg, no libmpv.** Demuxing, decoding, encoding, remuxing, subtitles and playback are our own Rust code, with the operating system's hardware decoders and encoders called directly.
5. **Insanely optimized.** Every hot path has a budget and a benchmark that fails CI when it regresses: idle memory, cold start, time to first frame, seek, scan of an unchanged library, and frames dropped. Zero-copy from hardware decoder to GPU texture to screen. No process spawned per stream. Nothing runs when nothing changes.

## Shape

Flicks has two programs: a **server** that owns folders of video, and a **client** that connects to any number of servers and shows them as one Netflix-style library. Both run on macOS, Windows and Linux. Neither needs anything we host: no account, no flicks.dev, no relay of ours. A server is reached at whatever address the user already has.

```text
  Flicks Server (Mac)       Flicks Server (Windows)      Flicks Server (Linux NAS)
  ~/Movies, ~/TV            D:\Media                     /srv/media
  Rune admin window         Rune admin window            headless: `flicks serve` + CLI
        ▲                         ▲                              ▲
        │ LAN 192.168.1.20        │ tailnet 100.71.3.9           │ OpenTunnel https://nas.example.com
        │                         │                              │ or iroh (no setup)
        └───────────────┬─────────┴──────────────────────────────┘
                        │ each server: its own key, own library, own device list
                 Flicks client (Rune)
                 one merged library, continue watching across all servers
```

- **Servers are independent.** Each has its own library, database and list of allowed devices. Servers don't need to know about each other. Adding a second server never changes the first.
- **The client consolidates.** It connects to every server you've added, merges their catalogs by TMDB id (the same film on two servers is one tile with two sources), picks the best source to play, and merges watch progress.
- **Core and thin UIs.** The server is a background service (launchd, Windows service, systemd) that runs without any window. The Rune admin window is just a client of its local API and can be closed. The same API serves the CLI.

## Reaching a server

A server is identified by its **key**, never by its address. Addresses are just ways to reach that key, and a server can have many:

| Address | Example | Transport |
|---|---|---|
| LAN | `https://192.168.1.20:7686` | HTTPS, self-signed certificate pinned to the server key |
| Tailnet | `https://100.71.3.9:7686`, `https://nas.tail1234.ts.net:7686` | Same; Tailscale also encrypts |
| Tunnel or reverse proxy | `https://nas.example.com` (OpenTunnel, Cloudflare Tunnel, Caddy) | The proxy's public certificate, plus the server-key check below |
| iroh | `iroh:<server key>` | QUIC, hole punching; no ports or setup |

The client tries every known address at once, keeps the first that proves it holds the server key, and prefers LAN, then tailnet, then tunnel, then iroh. iroh is optional: with it off, the server never contacts anyone. With it on, discovery and relays default to n0's public ones (or a self-hosted relay), never ours. Nothing in the protocol names flicks.dev, which is only the website.

## Authentication

Modelled on OpenCode's `opencode pair` (one-time code, QR with every reachable address, try them all, keep the first that answers), but with a key per device instead of one shared password, so the server can be put on the internet.

### Pairing

1. On the server (admin window, or `flicks pair` on a headless box) the admin opens a pairing window. It shows a QR code and a link:
   - QR payload: `{"v":1,"server":"<server key>","code":"<128-bit one-time code>","urls":["https://192.168.1.20:7686","https://nas.example.com","iroh:<key>"],"cert":"<sha256 of the LAN certificate>"}`
   - Link: `https://nas.example.com/pair#<code>.<server key>`. The code is in the fragment, so proxies and tunnels never log it.
   - The code is single-use and expires after 5 minutes.
2. The client scans or pastes it, generates its device key if it has none, and redeems the code at every address in parallel: it sends the code, its device public key and name, and checks that the server proves the key in the payload.
3. The server records the device (Deadbolt's signed `Add`) with a role (admin or viewer, and which libraries) and returns its other addresses. The client stores the server key and pins the LAN certificate.

For a TV or anything without a camera, the direction flips: the TV shows a short code (8 characters, 10 minutes), and the admin types it in the server's admin window or on a phone that's already an admin. It's the same exchange with the roles swapped, like Netflix's TV sign-in.

### Every request

- **Over iroh:** the QUIC handshake already authenticates both keys. The server checks the device is on its list on accept, and drops open connections when a device is removed (Deadbolt's membership gate, unchanged).
- **Over HTTPS:** the client asks for a challenge, signs `(nonce, server key, device key)` with its device key, and gets a **session token**. The token is short-lived (1 hour), names the device, and is MACed with a server secret. The server also signs the challenge with its own key, so a tunnel or DNS hijack can't pose as the server. Tokens are refreshed silently.
- **Media URLs** for players that can't send headers carry a signed, expiring query token scoped to one item. In practice the client core serves players from loopback, so tokens never reach the player.

### Why it's safe on the open internet

- **No password exists**, so there's nothing to guess or phish. A device is authorised only by holding its private key.
- **Only two routes skip auth:** a minimal `info` (server key and protocol version, no name or library) and pairing redemption. Both are rate-limited per IP and globally, and redemption only works while a pairing window is open.
- **Revocation is per device and immediate:** remove a device and its tokens stop working on the next request, and its iroh connections close.
- **Each device holds its own key**, so a leaked session token expires within the hour and a stolen laptop is removed without re-pairing everyone else.
- **The admin window on the server machine** authenticates with a local credential file readable only by that user (as OpenCode's service password works). That's also the recovery path: if every admin device is lost, `flicks admin reset` on the server machine pairs a new one.
- **Through a tunnel**, the tunnel operator can see traffic, because it terminates TLS. Use iroh (end-to-end encrypted) when that matters.

## Profiles

A person has one **profile**: a Deadbolt-style signed membership log of their devices (phone, laptop, TV). Pairing a new device into your profile, from a device you already have, gives it every server the profile can reach, so a new TV doesn't need pairing with each server. A server grants access to profiles; it keeps a copy of each granted profile's device log and checks it like Deadbolt's membership. Version 1 can start with device-only pairing and add profiles without changing the wire format, since a profile with one device is the same thing.

The profile log also carries the person's server list, watchlist and preferences, so a newly paired device sets itself up from its siblings.

## Watch progress

Progress is stored per (profile, item) on every server that holds that item, as a last-writer-wins value ordered by a hybrid logical clock. The client writes on start, pause, stop, a big seek, finishing, and every few minutes while playing. When two servers disagree, the client takes the newest and writes it back to the other. Servers never talk to each other; clients carry progress between them, and each device's own copy syncs across the profile.

## Server

- **Folders:** the admin adds local folders (or mounted network shares). The scanner walks them with folder hashes (an unchanged folder costs one directory listing), watches local disks for changes, reads headers with our own container parsers (keyframe index, codec data, HDR and Dolby Vision, audio formats), identifies files by OSHash, parses names with our own parser, and matches on TMDB.
- **Admin window (Rune):** folders and scan status, unmatched and wrong matches, editing metadata, devices and profiles (pair, rename, change role, remove), libraries and who can see them, transcoding (hardware detected, limits), addresses (LAN, tailnet, tunnel URL, iroh on or off), logs.
- **Playback:** one request (`/play/{item}?t=resume` with the client's device profile) answers with one of three paths:
  1. **Direct play:** byte ranges of the original file. No CPU. A seek is one range request. Every Flicks app takes this path for everything our player supports.
  2. **Remux:** fMP4 HLS cut from the keyframe index in process, for players that can't open the container (AVPlayer, browsers, some TVs).
  3. **Transcode:** our own pipeline in process, no child processes: hardware decode, scale before tone mapping on the GPU, hardware encode (VideoToolbox, Media Foundation with NVENC/QSV/AMF, VA-API), or our software H.264 encoder where there's no hardware. An audio-only transcode (DTS or TrueHD to AAC) leaves the video untouched.
- **Background analysis:** one pass per file for trickplay thumbnails, intro and credits markers, and loudness; results are keyed by content hash.
- **LAN discovery:** the server advertises `_flicks._tcp` through the OS's own DNS-SD responder (Bonjour, Avahi, Windows DNS-SD), so clients on the same network list it before pairing.

## Client

- **Library:** a home screen merged across servers (Continue Watching, Next Up, Recently Added, by library and genre), detail pages, search, versions and sources. Servers that are offline grey out their titles instead of hiding them.
- **Add server:** scan a QR, paste a link, or type an address and enter the server's short code.
- **Client core:** one Rust crate (`client`) holds the server list, address racing, auth, catalog and progress merge, and media fetching. Every Flicks app uses it: the Rune desktop app directly, and the iOS, tvOS and Android TV apps through a C ABI we generate ourselves.

## Media stack

All ours, in this repo. The starting point is FilmCraft's pure-Rust media code (no FFmpeg, checked frame by frame against ffmpeg), copied in and cut down to what playback and serving need; Isocast's platform layers and muxer are the starting point for hardware encode and MP4 writing.

| Area | Crate | Starts from | Then add |
|---|---|---|---|
| Containers | `container` | FilmCraft's Matroska (with Cues), MP4/MOV (fragmented), MPEG-TS readers; Isocast's MP4 writer | fMP4 HLS segmenting for the server's remux |
| Video decode | `video` | FilmCraft's H.264 (500–600 fps at 1080p in software), HEVC Main/Main 10 (~225 fps), AV1 (incl. film grain), VP9 | Dolby Vision (profiles 5, 7, 8) metadata and reshaping |
| Audio decode | `audio` | FilmCraft's AAC, Opus, AC-3; their FLAC/MP3/Vorbis go through a third-party crate there, so we write those | E-AC-3 and Atmos metadata, DTS, TrueHD, passthrough to receivers |
| Hardware | `platform` | FilmCraft's VideoToolbox, Media Foundation and VA-API decode; Isocast's macOS, Windows and Linux layers | VA-API AV1/VP9, hardware encode on all three for the server |
| Encode | `encode` | FilmCraft's H.264 and AAC encoders | Only for the server's transcodes |
| Subtitles | `subtitle` | — | SRT, WebVTT, PGS (bitmap, simple), ASS/SSA rendering on Rune's text engine |
| GPU | `gpu` | FilmCraft's YUV and colour code | Zero-copy hardware frame → wgpu texture on every OS, scale, HDR tone mapping, HDR output where the display supports it |

### Player

The `player` crate puts it together: demux, decode (hardware first, our software decoders when the hardware can't), audio output with A/V sync, subtitles, and frames as wgpu textures that Rune draws. It also builds for iOS, tvOS and Android, so every Flicks app plays the same files the same way, the way Infuse plays everything on Apple TV.

Until a gap closes (say DTS audio), the client asks the server for that one track transcoded (DTS to AAC) and plays the original video directly. Nothing ever falls back to a third-party player.

### Contributing it to Rune

Rune's `Player` is AVPlayer-only today, which means macOS, no MKV and no Windows or Linux. Once `player` is solid, it's a natural cross-platform backend for Rune's `Player`. We'd offer it upstream and keep Flicks on the same code.

## Apps and platforms

| Platform | App | Plays with |
|---|---|---|
| macOS, Windows, Linux | Flicks (Rune) | Our player |
| Server admin, all three | Flicks Server window (Rune) or CLI | — |
| iOS, iPadOS, tvOS | Flicks (SwiftUI shell + client core) | Our player; the system player only where Apple requires it (AirPlay, Picture in Picture) |
| Android, Android TV, Fire TV | Flicks (Compose shell + client core) | Our player |
| Anything else (Roku, LG, Samsung, Kodi, browsers) and until our apps ship | Infuse, Swiftfin, Streamyfin, Findroid, Jellyfin apps, jellyfin-web | Through the Jellyfin-compatible API |

### Jellyfin-compatible API

The server serves a subset of the Jellyfin 12 API (about 30–50 operations: login, views, items, resume, Next Up, PlaybackInfo, streams and HLS, images, sessions and progress, media segments), written from Jellyfin's OpenAPI spec. It merges nothing: each Jellyfin client sees the one server it points at.

Jellyfin clients sign in with a username and password or with **Quick Connect**, where the app shows a 6-character code and an already signed-in device approves it. Flicks uses Quick Connect only: the code is approved in the Flicks admin window or app, which matches our TV pairing, so there's still no password on the server. Jellyfin clients get a long-lived bearer token, which is weaker than a device key, so those tokens are:

- **viewer only**, never admin;
- **listed and revocable** per device, like any other device;
- **LAN and tailnet only by default**, with a setting to allow them from a tunnel.

## Crates

| Crate | Owns | Status |
|---|---|---|
| `core` | Device keys, signed entries, membership and roles, profile logs, progress registers | Copied from Deadbolt; strip password items, rename vault to profile |
| `store` | Encrypted redb for keys and logs | Copied from Deadbolt |
| `net` | iroh endpoint (tuned), membership gate, pairing, sync sessions | Copied from Deadbolt; add tuning and the new pairing payload |
| `container`, `video`, `audio`, `platform`, `encode`, `subtitle`, `gpu` | The media stack above | Copied in from FilmCraft and Isocast, then extended |
| `player` | Demux, decode, audio, A/V sync, subtitles, wgpu frames | New |
| `library` | Scanner, header parsing, OSHash, filename parsing, TMDB matching, catalog | New |
| `media` | Play planner, fMP4 HLS remux, transcode sessions, analysis | New |
| `server` | HTTPS API, auth and tokens, pairing windows, DNS-SD, service install, `flicks` CLI | New; daemon, local API and mutual-TLS patterns from Isocast |
| `jellyfin` | Jellyfin 12 API subset and Quick Connect | New |
| `client` | Server list, address racing, auth, catalog and progress merge, media fetching; C ABI for mobile | New |
| `app` | Rune client UI (library, player) | New |
| `admin` | Rune server admin window | New |

## Rune contributions

What Flicks needs from Rune, to build upstream rather than work around:

1. **Frames on Windows and Linux.** `NativeSurface` imports frames without copies only on macOS. The player needs zero-copy hand-off of a wgpu texture (from D3D11/D3D12 and VA-API/DMA-BUF) on every platform.
2. **A cross-platform `Player`.** Offer `player` as Rune's backend (above).
3. **Ten-foot UI.** Focus navigation by arrow keys, remotes and game controllers, and overscan-safe layouts, for the TV-style home screen on desktop.
4. **TV platforms.** tvOS and Android TV aren't on Rune's platform list, so those apps start as native shells around the Rust client core and player.

## Open questions

1. Ports: is 7686 free of common conflicts, and should the server try UPnP for a public port when iroh is off?
2. Live TV and DVR: later?
