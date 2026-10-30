# flicks

A lightweight, distributed alternative to Plex. Every machine in the house (and a friend's) shares one library, one watch history and one pool of transcoders, with no account server, no paywall on remote playback and no throttled relay.

Status: research. No code yet.

## Plan

- **Transport:** iroh, tuned (16–32 MiB stream windows, BBR, 2–4 parallel range streams), with a relay embedded in every server.
- **Playback:** direct byte-range play first; in-process fMP4 HLS remux from a scan-time keyframe index next; a pinned jellyfin-ffmpeg transcode on whichever household GPU has headroom last.
- **Library:** folder-hash rescans, in-process header parsing, OSHash file identity, SQLite in WAL mode with one writer.
- **Sync:** Deadbolt's signed membership log and QR pairing, plus a single-writer catalog feed per server and last-writer-wins watch progress.
- **Clients:** a Jellyfin 12 compatible API so Infuse, Swiftfin and the TV apps work from day one; a native client later for the multi-server features.
- **Extras:** trickplay, skip intro and credits, and loudness from one shared analysis pass per file, shared between servers by content hash.

## First slices

1. Benchmark and tune iroh on Linux and macOS.
2. Scanner with keyframe index.
3. QR pairing and direct play into a desktop mpv client.
4. Catalog and progress sync.
5. Jellyfin API layer and in-process HLS.

## Research

- [`research/2026-10-30-fast-stack.md`](research/2026-10-30-fast-stack.md): the fastest stack, layer by layer, with sources and the build order. Notes behind it are in [`research/2026-10-30-fast-stack/`](research/2026-10-30-fast-stack/).
- Sync design starts from [Deadbolt](../deadbolt/docs/design.md).
