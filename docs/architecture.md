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

- **Server key:** an ECDSA P-256 key, created on first run, stored in `server.key` (mode 0600; owner-only ACL on Windows). Windows data directories, keys, saved server lists and the database use a protected ACL granting only the current user and SYSTEM full control, with directory grants inherited by new files (including SQLite sidecars); existing sensitive files are restricted again on open. On every start the server mints a 365-day self-signed TLS certificate for that key in memory (nothing else on disk), and the app pins the key, not the certificate. P-256 because every TLS stack accepts it, including Apple's and Windows'.
- **Encodings:** public keys are SEC1 compressed (33 bytes), signatures are P-256 ECDSA `r ‖ s` (64 bytes). Every signed message starts with its own label (`flicks-…-v1\0`), so a signature for one purpose, including TLS, can never be replayed as another, and every variable-length field in it is length-prefixed (`lp(x)`: a u32 big-endian length, then `x`).
- **Probe first, always:** before anything else at an address (pairing included), the app sends `GET /info?probe=<nonce>`, which returns the server key, its name and `Sign(server key, "flicks-info-v1\0" ‖ lp(nonce) ‖ lp(name))`. Only an address whose answer verifies against the pinned key is used. Through a tunnel this is what proves the server is behind it: a tunnel can read traffic, but it can't impersonate the server, rename it or forge requests.
- **Which certificates count:** a certificate for the server key is accepted at any address (the server itself). Otherwise the address must be a hostname whose certificate is valid for that name under the public web roots (a tunnel or reverse proxy, which terminates TLS); a raw IP address accepts only the server-key certificate. The probe then checks the server either way.
- **Racing addresses:** the app tries the address that answered last time first and gives it 250 ms on its own, then races the rest alongside it and keeps the first whose probe verifies; that address is saved first for next time. A reachable server costs one probe (unauthenticated requests share one rate limit). Connecting, TLS and response headers get 5 s; a response body times out only after 10 s without a byte, so a large byte range over a slow uplink isn't cut off for being slow.
- **Name:** `/info` carries the server's name, the machine's name by default (the computer name on macOS, "Ryan's Mac"; the host name elsewhere) or `flicks serve --name`. The app saves it when pairing, updates it on connect, and shows it instead of an address; a name that isn't 1 to 64 bytes without control characters is replaced by the address's host.

## Authentication

Device keys and signed requests. No passwords, no bearer tokens.

### Pairing

1. The admin runs `flicks pair --user <name> [--admin] [--addr host:port]…` (or Add Device in the app). It opens a 5-minute pairing window and prints a QR code and a link containing the server key, a 128-bit one-time code and the server's addresses (any `--addr` tunnel or tailnet names first, so they're tried first, then its interface addresses). The CLI prints the link single-quoted, `flicks connect '<link>'`, since shells mangle its `?`, `&` and `[...]`; addresses can't contain `'`, and a link with no addresses is refused. While no window is open, the pairing route answers nothing.
2. The app creates its device key (P-256, in a `client.key` file, mode 0600, in a data directory kept at mode 0700 with its `servers.json` at 0600), races the link's addresses as above, and sends to the first that verifies: its public key and name, and `HMAC(HKDF(salt = "flicks pair v1" ‖ server key, ikm = code), transcript)`, signed with its device key. The code itself never travels, so whoever relays the request can't reuse it.
3. The server checks the HMAC and signature, adds the device to its `devices` table under that user, and answers with its own signature over `flicks-pair-ok-v1\0`, the transcript, the user's name and the admin flag. Redeeming twice with the same device key is a no-op; any other key gets "used".

### Every request

`Authorization: Flicks <device key id> <timestamp> <nonce> <signature>`, signing `flicks-req-v1\0`, then length-prefixed server key, method, path and query, and Range, then SHA-256(body), timestamp and nonce. The server checks, cheapest first: one `Authorization` and at most one `Range` header, the timestamp is within 60 s, the nonce is new, the key is a P-256 point, the device exists and isn't removed, then reads at most 64 KiB of body (within 10 s) and verifies. Byte-range requests for media are signed the same way. The device key id is the device's public key itself (base64url), the timestamp is Unix seconds, the nonce 16 random bytes; the exact byte layouts of this and the pairing messages are in `crates/client/src/auth.rs` and `pair.rs`, the one copy both sides use (the client signs, the server verifies).

- **Removing a device** (`flicks device rm`) takes effect on its next request.
- **Roles:** admin or viewer, per user. For now every viewer sees every folder; per-user library grants come later.
- **Folders:** adding one refuses system and secret locations (`/etc`, `~/.ssh`, the Flicks data dir) after resolving the path. On Windows this also covers Windows, Program Files, ProgramData, roaming AppData, and local AppData credential/configuration directories, but not the local Temp directory.
- **Local CLI:** admin commands on the server machine (`flicks folder`, `user`, `pair`, `device`) talk to the running server at `https://127.0.0.1:7686`, signing with `server.key` itself as the root admin. So there's one process writing the database, and recovery is just running `flicks pair --admin` on that machine. The admin routes live under `/admin/` (the app's Server screens use the same ones); `pair --admin` with no `--user` pairs to a user named `admin`.
- **Unauthenticated routes:** `/info` and pairing redemption (only while a window is open; otherwise it answers 401 like any unsigned request), and with `flicks serve --web <dir>` the web app's files (an unsigned `GET` for a file in that folder; `/` is `index.html`), all behind one global rate limit (bursts of 20, 10 a second).

## Server

- **Catalog:** folders are scanned with folder hashes (an unchanged folder costs one directory listing) and watched for changes (FSEvents, inotify and ReadDirectoryChangesW through `notify`). Each file's headers are read with `container`: codecs, duration, HDR flags, audio and subtitle tracks, and the keyframe index from Cues (MKV) or sample tables (MP4) without scanning the file. Files are identified by OSHash.
- **Matching:** names are parsed from paths (title, year, season, episode, edition), then matched on TMDB when a key is set (`FLICKS_TMDB_KEY`); artwork is cached once. Without a key everything still scans and plays under its parsed name. Tests use recorded TMDB responses, never the live API.
- **Storage:** one SQLite database (WAL), owned by `server`, which runs the single writer task and the migrations. `library` defines its catalog tables and works on a connection it's handed, so it has no database of its own.
- **API:** catalog (home rows, libraries, items, search), `/play/{file}` returning the file's facts and byte ranges for header and resume point in one response, byte-range serving, and progress (`PUT /progress/{item}`: position, played, updated-at; newest wins).
- **Remux (FLK-19):** signed `/hls/{file}/master.m3u8`, `/{track}.m3u8`, `/{track}/init.mp4` and `/{track}/{segment}.m4s` generate separate video/audio fMP4 renditions in process, without temporary files. The first version accepts indexed CFR MKV with H.264/HEVC and AAC-LC or independent AC-3/E-AC-3; cuts use catalog keyframes at least four seconds apart, keeping sub-second tails in the previous segment. Video preserves every PTS with DTS derived from DefaultDuration and the SPS reorder bound; the resulting presentation delay is shared with audio. No edit lists: drop whole audio priming/CodecDelay frames before presentation zero (the first retained frame may be one audio frame late). Audio after a seek may differ by at most one frame (23 ms for 44.1 kHz AAC, or the Dolby frame duration). Native players unable to sign obtain a five-minute file/device-scoped URL via signed `POST /hls/{file}/token`; device removal invalidates it immediately. Clients must refresh this grant before expiry. Other routes remain signature-only.
- **CLI:** `flicks serve`, `flicks folder add|rm|ls`, `flicks user add|ls`, `flicks pair`, `flicks device ls|rm`, `flicks status`. `flicks service install` writes a launchd agent on macOS and a systemd user service (with lingering, so it starts at boot) on Linux.

### Catalog API (FLK-11)

All routes below require the normal device signature, including the exact query and `Range`.
One file is one item: the positive integer ID is the same in `/items`, `/play`, `/files` and
`/progress`. Paths on the server never appear in these responses. Currently every paired user
can read every configured folder; per-user folder grants are not implemented yet. Removing a
folder makes its items inaccessible immediately; the worker deletes its catalog and progress rows.
Startup awaits table creation, then scans and watches in a per-server worker on its own SQLite
connection. Folder edits wake a full scan; watcher events rescan only the reported directories.
The idle worker blocks until an event or shutdown, with no polling timer.

| Request | JSON response |
|---|---|
| `GET /libraries` | `[{id, name}]` (one configured folder per library) |
| `GET /items`, `GET /search?q=title` | `[{item, progress}]` |
| `GET /home` | `{continue_watching: [{item, progress}], recently_added: [{item, progress}]}` |
| `GET /items/{id}` | `{item, streams, progress}` |
| `GET /play/{id}` | `{item, streams, keyframes: [{time_ms, offset}], progress, url: "/files/{id}", resume_ms, ranges: {header: [{start, end}], media: {start, end}}}` |
| `GET /artwork/{name}` | Cached JPEG bytes (not JSON) |
| `GET /progress` | `[{item: id, progress}]`, only the caller's saved records |
| `GET /progress/{id}`, `PUT /progress/{id}` | `{position, played, updated_at}` or `null` when not saved |

List routes (`items`, `search`, `home`, `progress`) accept `library=<id>`, `limit` (default 50,
clamped to 1–200) and `offset` (default 0); for home these apply to each row. Search `q` is
UTF-8 percent-encoded, with `+` also accepted for a space. Unknown query parameters are 400.
Items contain `id`, `title`, `year`, `season`, `episode`, `episode_end`, `edition`, `tmdb_id`,
`tmdb_media`, `poster`, `backdrop`, `added_at` (Unix seconds), `size`, `oshash`, `container`,
`duration_ms`, `video_codec`, `width`, `height`, `hdr`; unavailable facts are `null`. Artwork
values are local cache names (`w342-<id>.jpg` for posters, `w780-<id>.jpg` for backdrops), fetched
through signed `/artwork/{name}` requests. Clients never contact TMDB. Streams are audio/subtitle tracks in file order:
`{kind, codec, language, title, channels, default, forced}`; video facts are on the item.

Progress PUT takes exactly `{position, played, updated_at}`: nonnegative integer milliseconds
(position within the media; updated_at since Unix epoch). A strictly newer timestamp wins;
equal/older updates return the existing value. An updated_at more than 60 s ahead of the server's
clock gets 400, and clients ignore such values when merging, so one server can't freeze progress on others. The signed user selects the record, never a
request field. The local root key has no user and gets 403 on progress routes. Continue Watching
means positive position and not played, newest update first; Recently Added means newest scan
addition first. Missing/removed items return 404.

`/play` is the single **metadata** request before fetching media: header ranges cover MP4's
actual `ftyp`/`moov` (including tail `moov`) and box headers, or MKV's prefix and SeekHead-referenced
Info/Tracks/Cues/SeekHead/Chapters. Stale SeekHead entries are skipped; MP4 trailing padding and
boxes extending past EOF are tolerated just as at scan time. All byte ends are inclusive.
`media` is up to 1 MiB starting at the indexed
keyframe at/before progress (or the first keyframe when starting over; byte 0 if no index).
`resume_ms` is the target presentation time; played items restart at 0. Clients fetch the
listed ranges through signed `GET /files/{id}` and continue requesting media as needed.
No Range returns streamed 200; one closed, open-ended or suffix byte range returns 206 with
`Content-Range`, `Content-Length` and `Accept-Ranges: bytes`. Invalid, multiple or unsatisfiable
ranges return 416 with `Content-Range: bytes */<size>`. Files are streamed in at most 64 KiB
frames; every open rechecks containment with `library::resolve` and refuses symlinks from the
file up through its library root (not ancestors above that root). Scans skip all symlinks too.

## App

- **Screens:** Home (Continue Watching, Next Up, Recently Added, libraries), detail, search, player, Add Server (scan QR or paste link), and Server screens for admins (folders, users, devices, pairing).
  Home's rows after Continue Watching and Recently Added are TV Shows and Movies of the merged library (a show's episodes are one tile; the merged library has no per-server folders to make rows of). The desktop app shows the cached library at launch, refreshes it off the UI thread, and dims a title whose servers didn't answer the last refresh; it can't be played until one does.
- **Shared screens** (`ui`): the screens, their state and look, used by the desktop app and the web app; no I/O. Each app answers the screens' requests (refresh, pair, artwork, admin changes) with its own network: the desktop's network thread, the browser's fetches.
- **Web app** (`apps/web`, FLK-49): the same screens on Rune's web host (WebGPU), served by the server itself (`flicks serve --web`, the folder `apps/web/build.sh` makes), so every request is a same-origin `fetch` to the one server that served the page. Browsers don't trust the server's self-signed certificate, so it's reached through an HTTPS front with a real certificate (a tailnet name, a tunnel, a reverse proxy); the `/info` probe still proves the server behind it. Requests are signed exactly as `client` signs them (its message bytes, `auth::header`), with a non-extractable WebCrypto P-256 key kept in IndexedDB with the paired server; pairing takes a pasted `flicks://pair` link whose key must be this page's server. No playback in the browser yet (that needs CMAF segments and MSE).
- **Client core** (`client`): server list, keys and request signing (shared with the server and its local CLI), address racing (see "Reaching a server"), catalog merge across servers, progress merge (newest wins, written back to servers that are behind).
- **Merged library** (`client::merge`, FLK-40): files are one title when TMDB matches (a movie's id; a series id with season and episode), otherwise by parsed title, year, season and episode; an unmatched copy joins a matched title of the same name. The source to play is on a server that answered, then has the most pixels, then the largest file, then the lowest probe latency. Each server's last catalog is cached in `catalog.json` in the data directory, so a server that doesn't answer keeps its titles, unavailable. Every server is refreshed at once, so servers that don't answer cost one connect timeout together.
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
| `client` | Server list, signing, address racing, merge (keys, messages and merge also build for wasm) |
| `ui` | The app's screens (Rune), shared by `apps/desktop` and `apps/web` |
| `platform` | OS hardware decode (the only crate with `unsafe`, with `gpu` and later `ffi`) |
| `gpu` | wgpu video renderer |
| `audio` | Channel layout, downmix, resample, output |
| `player` | Playback engine |
| `subtitle` | Subtitle parsing and layout |
| `apps/desktop` | The Rune app on macOS (later Linux, Windows): the `ui` screens in a window |
| `apps/web` | The same screens in the browser, served by the server |
| `core`, `net`, `store` | Deadbolt's signed membership, iroh sync and encrypted storage. Not used by the server or app yet; kept for profiles across servers and iroh later |

## Open questions

1. Ship a Flicks TMDB API key in the binary (as Jellyfin does), or ask each server owner for one?

## Later (not designed yet, on purpose)

Profiles across servers, iroh connections, TVs and phones (tvOS, iOS, Android) with a TV pairing flow, the Jellyfin-compatible API, HLS remux and transcoding, Dolby Vision, HDR output, DTS, TrueHD, passthrough, more audio codecs, PGS and ASS subtitles, skip intro and trickplay, LAN discovery, offline downloads, Live TV. Each gets designed when its milestone comes.
