# Flicks architecture

2026-10-30. Second draft. No product code yet; `crates/core`, `crates/net` and `crates/store` are copied from Deadbolt (`../deadbolt`, commit 7595d31) and renamed. Research: `research/2026-10-30-fast-stack.md`.

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

- **Folders:** the admin adds local folders (or mounted network shares). The scanner walks them with folder hashes (an unchanged folder costs one directory listing), watches local disks for changes, reads headers in-process (keyframe index, codec data, HDR and Dolby Vision, audio formats), identifies files by OSHash, parses names with `hunch`, and matches on TMDB.
- **Admin window (Rune):** folders and scan status, unmatched and wrong matches, editing metadata, devices and profiles (pair, rename, change role, remove), libraries and who can see them, transcoding (hardware detected, limits), addresses (LAN, tailnet, tunnel URL, iroh on or off), logs.
- **Playback:** one request (`/play/{item}?t=resume` with the client's device profile) answers with direct play (byte ranges of the original file), an in-process fMP4 HLS remux from the keyframe index, or a `jellyfin-ffmpeg` transcode with hardware acceleration. Direct play costs no CPU, and a seek is one range request.
- **Background analysis:** one pass per file for trickplay thumbnails, intro and credits markers, and loudness; results are keyed by content hash.

## Client

- **Library:** a home screen merged across servers (Continue Watching, Next Up, Recently Added, by library and genre), detail pages, search, versions and sources. Servers that are offline grey out their titles instead of hiding them.
- **Player:** the client core (Rust) fetches from the chosen server over the best address and serves the player from loopback. On macOS that's Rune's `Player` (AVPlayer: MP4, MOV, HLS; MKV goes through the server's remux). Windows and Linux need a player backend in Rune first (below).
- **Add server:** scan a QR, paste a link, or type an address and enter the server's short code.

## Crates

| Crate | Owns | Status |
|---|---|---|
| `core` | Device keys, signed entries, membership and roles, profile logs, progress registers | Copied from Deadbolt; strip password items, rename vault to profile |
| `store` | Encrypted redb for keys and logs | Copied from Deadbolt |
| `net` | iroh endpoint (tuned), membership gate, pairing, sync sessions | Copied from Deadbolt; add tuning and the new pairing payload |
| `library` | Scanner, header parsing, OSHash, TMDB matching, SQLite catalog | New |
| `media` | Play planner, fMP4 HLS remux, ffmpeg supervisor, analysis | New |
| `server` | HTTPS API, auth and tokens, pairing windows, service install, `flicks` CLI | New |
| `client` | Server list, address racing, catalog merge, progress merge, loopback media proxy | New |
| `app` | Rune client UI (library, player) | New |
| `admin` | Rune server admin window | New |

## Rune dependencies

To report to Rune, not work around:

1. **Video on Windows and Linux.** Rune's `Player` uses AVPlayer and reports "video playback isn't supported on this platform yet" elsewhere. Flicks needs a backend there; libmpv rendering into a `NativeSurface` would also play MKV directly.
2. **TV platforms.** tvOS and Android TV aren't on Rune's platform list. Until they are, TVs use existing Jellyfin apps through a Jellyfin-compatible API on the server.

## Open questions

1. Ports: is 7686 free of common conflicts, and should the server try UPnP for a public port when iroh is off?
2. Jellyfin-compatible API in version 1, or after our own TV client exists?
3. Live TV and DVR: later?
