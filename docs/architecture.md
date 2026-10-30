# Flicks architecture

2026-10-30. Fifth draft: simplified, ordered by platform. Research: `research/2026-10-30-fast-stack.md` (much of it is superseded by this doc).

## Principles

1. **Simple first.** Build the smallest thing that works, with defaults that need no configuration. No knobs, modes or extra listeners until a real user needs them. Optimize what a measurement shows is slow, not what might be.
2. **One platform at a time.** macOS server, then macOS client, then Linux server, Linux client, Windows, web, then everything else. A milestone is done when it works end to end on real hardware.
3. **Everything lives in this repo.** No git dependencies on other projects; code we reuse (FilmCraft, Deadbolt) is copied in with its notice in `THIRD_PARTY_NOTICES`. Rune is the one exception (pinned rev, `default-features = false`), and Rune gaps get fixed in Rune.
4. **Foundation crates from crates.io are fine** (tokio, rustls, wgpu, SQLite bindings, audio output, OS bindings). Media, playback, library and protocol code is ours. No FFmpeg, no libmpv, no GPL; ffmpeg is only a test oracle.
5. **Fast by measurement.** Each card that touches a hot path records its numbers in `docs/perf.md` (time to first frame, seek, unchanged rescan, decode fps, idle memory). Direct play costs no server CPU; frames stay on the GPU.

## Shape

```text
  Flicks server (macOS, later Linux, Windows)        Flicks app (macOS, later Linux, Windows, web)
  `flicks serve`: folders → SQLite catalog            one library merged from every server you've added
  one HTTPS port (7686)               ◀── LAN / tailnet / tunnel ──▶   our own player
```

- **Server:** one Rust binary (`flicks`), one HTTPS port, one SQLite database, one key file. Managed with the CLI, and from the app's Server screens when your device is an admin.
- **App:** a Rune desktop app. It connects to any number of servers and merges their catalogs by TMDB id; the same film on two servers is one tile with two sources.
- **Servers never talk to each other.** Nothing we host is involved: no account, no relay, no flicks.dev.

## Reaching a server

A server has one HTTPS listener. The user reaches it however they like: LAN address, tailnet address or name, or a tunnel or reverse proxy (OpenTunnel, Cloudflare Tunnel, Caddy) pointed at it. The server doesn't care which; every request is authenticated the same way, so a tunnel needs no special handling.

- **Server key:** an ECDSA P-256 key, created on first run, stored in `server.key` (mode 0600; owner-only ACL on Windows). On every start the server mints a 365-day self-signed TLS certificate for that key in memory (nothing else on disk), and the app pins the key, not the certificate. P-256 because every TLS stack accepts it, including Apple's and Windows'.
- **Encodings:** public keys are SEC1 compressed (33 bytes), signatures are P-256 ECDSA `r ‖ s` (64 bytes). Every signed message starts with its own label (`flicks-…-v1\0`), so a signature for one purpose, including TLS, can never be replayed as another, and every variable-length field in it is length-prefixed (u32, big-endian).
- **Through a tunnel** the proxy terminates TLS, so the app checks the server instead with a signed probe: `GET /info?probe=<nonce>` returns the server key and `Sign(server key, "flicks-info-v1" ‖ nonce)`. A tunnel can read traffic, but it can't impersonate the server or forge requests.

## Authentication

Device keys and signed requests. No passwords, no bearer tokens.

### Pairing

1. The admin runs `flicks pair --user <name> [--admin]` (or Add Device in the app). It opens a 5-minute pairing window and prints a QR code and a link containing the server key, a 128-bit one-time code and the server's addresses. While no window is open, the pairing route answers nothing.
2. The app creates its device key (P-256, in a `client.key` file, mode 0600) and sends, to every address at once: its public key and name, and `HMAC(HKDF(salt = "flicks pair v1" ‖ server key, ikm = code), transcript)`, signed with its device key. The code itself never travels, so whoever relays the request can't reuse it.
3. The server checks the HMAC and signature, adds the device to its `devices` table under that user, and answers with its own signature over `flicks-pair-ok-v1\0` and the transcript. Redeeming twice with the same device key is a no-op; any other key gets "used".

### Every request

`Authorization: Flicks <device key id> <timestamp> <nonce> <signature>`, signing `flicks-req-v1\0`, then length-prefixed server key, method, path and query, and Range, then SHA-256(body), timestamp and nonce. The server checks, in order: the device exists and isn't removed, the timestamp is within 60 s, the nonce is new, then reads at most 64 KiB of body and verifies. Byte-range requests for media are signed the same way.

- **Removing a device** (`flicks device rm`) takes effect on its next request.
- **Roles:** admin or viewer, per user; viewers see only the libraries their user is given.
- **Folders:** adding one refuses system and secret locations (`/etc`, `~/.ssh`, the Flicks data dir) after resolving the path.
- **Local CLI:** admin commands on the server machine (`flicks folder`, `user`, `pair`, `device`) talk to the running server at `https://127.0.0.1:7686`, signing with `server.key` itself as the root admin. So there's one process writing the database, and recovery is just running `flicks pair --admin` on that machine.
- **Unauthenticated routes:** `/info` and pairing redemption (only while a window is open), both rate-limited globally.

## Server

- **Catalog:** folders are scanned with folder hashes (an unchanged folder costs one directory listing) and watched for changes (FSEvents, later inotify and ReadDirectoryChangesW). Each file's headers are read with `container`: codecs, duration, HDR flags, audio and subtitle tracks, and the keyframe index from Cues (MKV) or sample tables (MP4) without scanning the file. Files are identified by OSHash.
- **Matching:** names are parsed from paths (title, year, season, episode, edition), then matched on TMDB when a key is set (`FLICKS_TMDB_KEY`); artwork is cached once. Without a key everything still scans and plays under its parsed name. Tests use recorded TMDB responses, never the live API.
- **Storage:** one SQLite database (WAL), owned by `server`, which runs the single writer task and the migrations. `library` defines its catalog tables and works on a connection it's handed, so it has no database of its own.
- **API:** catalog (home rows, libraries, items, search), `/play/{file}` returning the file's facts and byte ranges for header and resume point in one response, byte-range serving, and progress (`PUT /progress/{item}`: position, played, updated-at; newest wins).
- **CLI:** `flicks serve`, `flicks folder add|rm|ls`, `flicks user add|ls`, `flicks pair`, `flicks device ls|rm`, `flicks status`. On macOS, `flicks service install` writes a launchd agent.

## App

- **Screens:** Home (Continue Watching, Next Up, Recently Added, libraries), detail, search, player, Add Server (scan QR or paste link), and Server screens for admins (folders, users, devices, pairing).
- **Client core** (`client`): server list, request signing, address racing (try every known address, keep the first whose `/info` probe verifies), catalog merge across servers, progress merge (newest wins, written back to servers that are behind).
- **Player** (`player`): reads byte ranges from a server (with prefetch) or a local file; demuxes with `container`; decodes video with hardware first (VideoToolbox on macOS) and our software decoders as fallback; decodes audio with ours; audio is the master clock; frames are drawn by `gpu`.
- **Drawing video:** `gpu` is a plain wgpu renderer (no Rune dependency; wgpu pinned to Rune's version, `~30.0.1`): YUV (NV12, P010) to display colour, scaling, HDR tone-mapped to SDR, respecting Rune's `srgb_blend`. The app hosts it through Rune's `Compositor` hook with `El::effect` and `Renderer::draw_with`, so video is drawn in Rune's own pass on Rune's device, with the UI over it.
- **Audio** (`audio`): one canonical channel order across decoders, stereo downmix (ITU-R BS.775), resampling to the device rate, dynamic range compression off, device output.
- **Subtitles:** SRT and WebVTT first.

## Crates

| Crate | What |
|---|---|
| `container`, `mpegts` | Bit I/O, MKV/WebM, MP4/MOV, MPEG-TS (from FilmCraft and craft-libs) |
| `h264`, `hevc`, `av1`, `vp9`, `aac`, `ac3`, `opus` | Decoders (from FilmCraft) |
| `testkit` | Test helpers, ffmpeg oracle discovery |
| `library` | Scanner, probe, OSHash, catalog in SQLite |
| `metadata` | Filename parser, TMDB matching, artwork |
| `server` | HTTPS, auth, pairing, API, CLI (`flicks` binary) |
| `client` | Server list, signing, address racing, merge |
| `platform` | OS hardware decode (the only crate with `unsafe`, with `gpu` and later `ffi`) |
| `gpu` | wgpu video renderer |
| `audio` | Channel layout, downmix, resample, output |
| `player` | Playback engine |
| `subtitle` | Subtitle parsing and layout |
| `apps/desktop` | The Rune app |
| `core`, `net`, `store` | Deadbolt's signed membership, iroh sync and encrypted storage. Not used by the server or app yet; kept for profiles across servers and iroh later |

## Open questions

1. Ship a Flicks TMDB API key in the binary (as Jellyfin does), or ask each server owner for one?

## Later (not designed yet, on purpose)

Profiles across servers, iroh connections, TVs and phones (tvOS, iOS, Android) with a TV pairing flow, the Jellyfin-compatible API, HLS remux and transcoding, Dolby Vision, HDR output, DTS, TrueHD, passthrough, more audio codecs, PGS and ASS subtitles, skip intro and trickplay, LAN discovery, offline downloads, Live TV. Each gets designed when its milestone comes.
