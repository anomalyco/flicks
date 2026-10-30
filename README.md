# flicks

A lightweight Plex alternative. Run a Flicks server on any Mac, Windows or Linux machine pointed at your folders; the Flicks app connects to any number of servers and shows them as one Netflix-style library. No account, no paywall, nothing we host: reach a server on your LAN, your tailnet, a tunnel, or peer to peer.

Status: design. Device keys, pairing and sync are copied from Deadbolt; no Flicks product code yet.

Everything is built here, in Rust, with every UI in Rune. No FFmpeg, no libmpv, no dependencies on other projects' repos. See [`docs/architecture.md`](docs/architecture.md).

## First slices

1. Media stack: copy in FilmCraft's containers and decoders, cut to playback, with benchmarks.
2. Player in Rune: direct-play an MKV from disk with hardware decode, A/V sync and seeking, on macOS first.
3. Server: folders, scanner with keyframe index, HTTPS API with device-key pairing, Rune admin window.
4. Client: add servers by any address, merged library, progress across servers.
5. Jellyfin-compatible API, then remux and transcode for clients that need them.

## Docs

- [`docs/architecture.md`](docs/architecture.md): nodes, crates, state streams, playback, remote access, friends and open questions.

## Research

- [`research/2026-10-30-fast-stack.md`](research/2026-10-30-fast-stack.md): the fastest stack, layer by layer, with sources and the build order. Notes behind it are in [`research/2026-10-30-fast-stack/`](research/2026-10-30-fast-stack/).
- Sync design starts from [Deadbolt](../deadbolt/docs/design.md).
