# flicks

A lightweight Plex alternative. Run a Flicks server on any Mac, Windows or Linux machine pointed at your folders; the Flicks app connects to any number of servers and shows them as one Netflix-style library. No account, no paywall, nothing we host: reach a server on your LAN, your tailnet, a tunnel, or peer to peer.

Status: early. Device keys, pairing and sync (from Deadbolt) and the containers and decoders (from FilmCraft) are copied in; no Flicks product code yet.

Everything is built here, in Rust, with every UI in Rune. No FFmpeg, no libmpv, no dependencies on other projects' repos.

## Docs

- [`docs/architecture.md`](docs/architecture.md): servers, clients, auth, media stack, apps and crates.
- [`ROADMAP.md`](ROADMAP.md): the work, as cards agents take in parallel.
- [`AGENTS.md`](AGENTS.md): rules and the parallel workflow.
- [`apps/README.md`](apps/README.md): where the desktop, admin, Apple and Android apps live.

## Research

- [`research/2026-10-30-fast-stack.md`](research/2026-10-30-fast-stack.md): the fastest stack, layer by layer, with sources and the build order. Notes behind it are in [`research/2026-10-30-fast-stack/`](research/2026-10-30-fast-stack/).
- Sync design starts from [Deadbolt](../deadbolt/docs/design.md).
