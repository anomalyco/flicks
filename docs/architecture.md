# Flicks architecture

2026-10-30. First draft, built from `research/2026-10-30-fast-stack.md`. No code yet. Sync and pairing start from Deadbolt (`../deadbolt/docs/design.md`).

## Shape

One Rust binary, `flicks`, runs on every machine. A running instance is a **node**. Nodes that belong to the same people form a **household**, and every node in it sees one merged library, one watch history and one pool of transcoders. There is no account server: identity is a device key, and joining is QR pairing from a device that's already in.

```text
                      household (signed membership log)
   ┌──────────────────────────────────────────────────────────────────┐
   │                                                                  │
   │  NAS node             Desktop node            Mac mini node      │
   │  library, relay       library, GPU (NVENC)    GPU (VideoToolbox) │
   │  ┌───────────┐        ┌───────────┐           ┌───────────┐      │
   │  │  flicks   │◀──────▶│  flicks   │◀─────────▶│  flicks   │      │
   │  └─────▲─────┘  iroh  └─────▲─────┘   iroh    └───────────┘      │
   │        │ full mesh between servers, LAN fast path over TCP       │
   └────────┼────────────────────┼────────────────────────────────────┘
            │ iroh (direct, or a home relay)
            │                    │ Jellyfin 12 API over HTTP
   ┌────────┴───────┐   ┌────────┴──────────────────────────────┐
   │ Flicks app     │   │ Infuse, Swiftfin, Jellyfin Android TV, │
   │ Rust core +    │   │ jellyfin-web, Kodi                     │
   │ AVPlayer/mpv   │   └───────────────────────────────────────┘
   └────────────────┘
      friend's household ──grant──▶ reads one library over a home relay
```

Roles are capabilities a node advertises, not node types: **library** (holds files, scans them), **transcode** (measured fps budget per codec path), **relay** (reachable port, runs `iroh-relay`), **always-on**. A NAS can be all four; a laptop may be none and still be a full member.

## Crates

Same layout as Deadbolt. Membership, pairing and the sync session are lifted from Deadbolt rather than rewritten.

| Crate | Owns |
|---|---|
| `core` | Signed logs and their rules: membership and grants, catalog feed, progress registers, analysis results. HLC, checkpoints, pruning. No I/O. |
| `store` | SQLite (catalog, keyframe index, metadata, search) with one writer task and a reader pool; redb for the signed logs. |
| `net` | Tuned iroh endpoint (16–32 MiB stream windows, 64 MiB connection, BBR, 2–4 range streams), embedded relay, membership gate on accept and dial, the ALPNs below, LAN TCP fast path. |
| `scan` | Folder-hash walker plus `notify`, MKV/MP4 header parsing (keyframe index, codec-private data, init segment, HDR/DV/Atmos flags), OSHash, `hunch` filename parsing, TMDB matching. |
| `media` | Play planner, in-process fMP4/HLS remux, `jellyfin-ffmpeg` supervisor and argv compiler, the fused analysis pass. |
| `api` | Local HTTP API on loopback (the app and CLI use it) and the Jellyfin 12 facade on the LAN. |
| `cli` | The `flicks` binary: `flicks node` daemon, commands on the local API, deterministic simulator. |
| `clients/*` | Thin platform shells over a shared Rust client core: desktop (libmpv), Apple TV/iOS (AVPlayer, MPVKit fallback), Android TV later. |

### ALPNs

| ALPN | Use |
|---|---|
| `flicks/pair/1` | QR pairing, as Deadbolt's. |
| `flicks/sync/1` | Heads or cursor exchange per stream (below), plus an eager `NOTIFY` on open connections. |
| `flicks/range/1` | Byte ranges of a source file by content id. Playback, seeks and remote transcode input. |
| `flicks/segment/1` | Transcoded fMP4 segments pushed from a transcode node to the session owner, checksummed. |
| `iroh-blobs` | Only for copying files: offline downloads, replication, multi-source fetch. |

## State

Split by write pattern. One log for everything would drown in progress ticks.

| Stream | Writer | Shape | Sync |
|---|---|---|---|
| **Membership and grants** | Any member | Deadbolt's signed causal `Add`/`Remove` log, plus `Grant{friend, library, expires}` / `RevokeGrant`, plus `Profile` entries. Never pruned. | Heads exchange, always first. Gate checks are O(1) set lookups. |
| **Catalog** | The node that holds the files | Per-node sequence feed of `Source{content_id, item_id, path, size, media facts}` upserts and tombstones. | "Changes since N" (one round trip); negentropy reconciliation when the cursor is behind the tombstone horizon, and as periodic anti-entropy. |
| **Metadata** | Any node; the first to fetch | TMDB records and artwork refs keyed by TMDB id, refreshed within TMDB's 6-month cache limit. | Same as catalog. Artwork bytes are fetched on demand. |
| **Progress** | The device that played | LWW register per (profile, item), ordered by HLC with device id as tie-break. `played` is its own register. | Signed op only on start, pause, stop, a seek over 30 s, crossing the watched point and every 2–5 min. 1 Hz now-playing is ephemeral and unsigned. Checkpoints prune old ops. |
| **Analysis results** | Whichever node ran the job | Keyed by `(content_id, analyzer, version, params)`: segments (intro, credits...), loudness, trickplay ref, subtitle refs. | Small results replicate; big ones (tiles, subtitles) are fetched by hash. |

Identity:

- **Device:** Ed25519 key, also its iroh endpoint id (Deadbolt).
- **Content:** OSHash plus size (128 KB read per file). Full BLAKE3 only when a file is copied.
- **Item:** TMDB id (episodes: series id + season + episode). The same film on two servers is one item with two sources.
- **Profile:** a person in the household, separate from devices. Progress belongs to profiles.

## Playback

1. The app (or the Jellyfin facade) calls `/play/{item}?t=resume` on any node, with its device profile.
2. The planner picks a source: same LAN, then one the client can direct play, then the least loaded.
3. It answers in one round trip with one of three paths:
   1. **Direct play:** the source node and the byte ranges of header, index and resume point. Zero server CPU.
   2. **Remux:** an HLS playlist plus a ready init segment. Segment N comes straight from the keyframe index, in process, so a seek is just another segment number.
   3. **Transcode:** a session on the node with the most fps headroom that can do the job. It reads the source over `flicks/range/1`, scales before tone mapping, and pushes segments back. Segment boundaries are fixed, so a seek or failover restarts at segment N on any node. Throttled by pausing ffmpeg when 60–180 s ahead.
4. Bytes go from the source (or transcode) node to the client. Nothing routes through a coordinator.

Scan-time work removes most transcodes: an EAC3/AAC companion track for TrueHD/DTS titles, PGS-to-text OCR, and honest device-profile answers.

## Remote access

- Clients pre-dial every known node at launch and keep one connection each, so playback never waits on hole punching.
- About 70–90% of remote pairs go direct. The rest go through a **home relay**: any node with a reachable port runs `iroh-relay` with raised per-client limits. An optional VPS relay covers households with no reachable node.
- Third-party Jellyfin apps reach the facade on the LAN, or remotely through a forwarded port.
- Web: browser iroh is relay-only, so the web client uses HTTPS range requests to a reachable node and demuxes MKV itself with Mediabunny.

## Friends

A friend is another household. The owner adds `Grant{friend household, library, expires}` to their membership log. The friend's nodes can then sync that library's catalog slice and read its ranges; the serving node checks the grant on every request. Revoking cuts them off within one sync round. The friend's progress stays in the friend's own household and is never visible to the owner.

## Background work

All analysis for a file runs as **one pass** over it: keyframes feed trickplay tiles and credits black-frame detection; audio feeds loudness, Chromaprint and a speech timeline. That's under a CPU-minute per hour of video, excluding speech-to-text. Jobs are leased through replicated state and placed on the node that holds the file first, then idle AC-powered nodes, then by capability (GPU or Neural Engine for speech-to-text). Results are keyed by content hash, so each file is analysed once per household. Intro and credits markers check chapters, then TheIntroDB/IntroDB, then local Chromaprint, and are emitted only at high confidence.

## Clients

- **Third-party, day one:** the Jellyfin 12 facade, about 30–50 operations (auth, views, items, resume, Next Up, PlaybackInfo, stream and HLS, images, sessions/progress, media segments, SyncPlay later). Built from the OpenAPI spec; Ferrofin is GPL, so read it, don't copy it.
- **Our app, later:** a shared Rust client core embeds iroh, pre-dials, fetches ranges from the best source, and serves them to the platform player on loopback. AVPlayer or ExoPlayer first (power, AirPlay, frame-rate and HDR matching), libmpv as the fallback. Only this app gets multi-source fetching and remote play without port forwarding.
- **CLI:** `flicks` drives the same local API (pair, scan, play URLs, status, jobs).

## Testing

Extend Deadbolt's deterministic simulator with: LWW convergence after a prune, catch-up from behind a checkpoint, revocation stopping catalog flow within one round, and bounded catch-up bytes after a four-week partition. Transport and playback numbers come from benchmarks against real hardware (slice 1 and 3 in the README).

## Open questions

1. **Share or fork Deadbolt's code?** Extracting membership, pairing and the sync session into a shared crate keeps one audited implementation; forking is faster now.
2. **App UI:** Rune for desktop and TV shells, or native SwiftUI/Compose around the shared Rust core?
3. **Web client reachability:** HTTPS to a home node needs a certificate and a name. Per-household subdomains of flicks.dev would need a small DNS service, which is the only server we'd run.
4. **Metadata licensing:** TMDB and TVDB terms limit commercial use.
5. **Live TV/DVR:** in scope for v1 or later?
