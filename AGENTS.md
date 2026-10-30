# Flicks

A lightweight Plex alternative: Flicks servers (macOS, Windows, Linux) own folders of video, and Flicks apps connect to any number of servers and show them as one library. Rust everywhere, every UI in Rune, no FFmpeg or libmpv, nothing we host. Design: [`docs/architecture.md`](docs/architecture.md). Plan: [`ROADMAP.md`](ROADMAP.md).

## Layout

| Path | What |
|---|---|
| `crates/container`, `mpegts` | MKV/WebM, MP4/MOV and MPEG-TS demuxers and muxers, bit I/O (from FilmCraft and craft-libs) |
| `crates/h264`, `hevc`, `av1`, `vp9`, `aac`, `ac3`, `opus` | Decoders (from FilmCraft); `ac3` also does E-AC-3 |
| `crates/testkit` | Test-only helpers: ffmpeg/ffprobe oracle discovery, fixtures, golden images |
| `crates/core`, `net`, `store` | Deadbolt's signed membership, iroh sync, encrypted storage. Not used yet; leave them alone unless a card says otherwise |
| `crates/<new>` | Every new crate, named `flicks-<dir>`; the workspace picks it up with no root edit |
| `apps/` | Apps ([`apps/README.md`](apps/README.md)) |
| `docs/` | Design; `docs/perf.md` holds measured numbers |
| `research/` | Dated research behind decisions |
| `THIRD_PARTY_NOTICES`, `licenses/` | Notices for copied code |

## Rules

0. **Simple first.** Build the smallest version that works and meets the card's done-when. No configuration options, modes, feature flags, traits or abstractions the card doesn't need. If you're tuning or generalizing something no measurement or user asked for, stop. Reviews will ask you to delete it.
1. **Everything lives in this repo.** Never add a git dependency on another project. To reuse code from FilmCraft, Isocast, Deadbolt or anything else, copy it into a crate here, rename it to `flicks-*`, and add its copyright and licence to `THIRD_PARTY_NOTICES` in the same commit. **Rune is the only exception**: depend on it by pinned rev with `default-features = false` (its defaults pull in third-party media crates, which `deny.toml` bans).
2. **Foundation crates from crates.io are fine** (tokio, iroh, rustls, wgpu, redb, SQLite bindings, text shaping, compression, OS bindings). Media, playback, library and protocol code is ours. `deny.toml` is the licence policy (permissive licences, plus MPL-2.0 for unmodified foundation crates) and bans FFmpeg, mpv, GStreamer and third-party codec crates. Each new dependency gets a line in the commit message saying why.
3. **No FFmpeg, no libmpv, no GPL code.** ffmpeg and ffprobe are allowed only as external test oracles and fixture generators through `flicks-testkit`, never linked or shipped. Never copy GPL or LGPL code (FFmpeg, mpv, x264, x265, VLC). Write codecs and formats from the published specifications, and name the spec edition in the crate README.
4. **Never crash.** Media files, network input and settings are hostile. Return `Result`, never `unwrap`, `expect`, `panic!`, `todo!` or `unreachable!` outside tests (every crate's `lib.rs` carries `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]`; add it to new crates). Index with `get`, check arithmetic on lengths and offsets, cap allocations sized by input, bound loops and recursion.
5. **`unsafe` only in `crates/platform` and `crates/gpu`**, on their FFI modules, each block with a `// SAFETY:` comment, behind a safe `Result` API. The workspace denies `unsafe_code`, and every other crate's `lib.rs` carries `#![forbid(unsafe_code)]` (add it to new crates).
6. **Fast by measurement.** A card that touches a hot path (open, seek, decode, first frame, scan, idle memory) records its numbers in `docs/perf.md`, and later changes to that path put before and after numbers in the commit message. Optimize what the numbers show, nothing else.
7. **Rune gaps get fixed in Rune.** If Flicks needs something Rune doesn't have, build it in Rune (a PR there, per Rune's own AGENTS.md) rather than working around it here. Note the Rune PR on the card.
8. **Tests.** Prove decoder, parser and protocol changes with the existing conformance, oracle and fuzz suites, adding cases to existing test files. Parsers of untrusted input get fuzz cases (truncation, bit flips, bad sizes). Test behaviour, never source text.

## Working in parallel

Agents work one card at a time from `ROADMAP.md`, each in its own worktree, tracked on the board: the `flicks` project (cards `FLK-n`), http://127.0.0.1:8789/p/flicks. Ryan reads the board, not chat.

1. **Pick a card** that is in `todo` on the board (`board ls --status todo`), isn't assigned, and whose **needs** are done. Ask the coordinator if unsure.
2. **Name yourself and claim it:** `export BOARD_PROJECT=flicks BOARD_BY="<model> · FLK-n"` (worktrees sit outside the linked repo, so `BOARD_PROJECT` is required), then `board set FLK-n --assignee "$BOARD_BY"` and `board mv FLK-n progress`.
3. **Make a worktree:** `git worktree add ../flicks-FLK-n -b flk-n main`, and work only there.
4. **Stay inside the card's owned paths.** Shared files (root `Cargo.toml`, `Cargo.lock`, `AGENTS.md`, `ROADMAP.md`, `docs/architecture.md`, `THIRD_PARTY_NOTICES`) change only when the card needs it, in a separate small commit, and the final note says so. Only the coordinator edits `ROADMAP.md`.
5. **Note milestones** in one line with numbers: `board note FLK-n "4K HEVC 10-bit: 142 fps, matches software on 41/41 clips"`. Blocked: `board set FLK-n --blocked "reason"`, then do something else. Run `board check` between steps and obey it.
6. **Hand back:** `scripts/check.sh` passes, commits are made, then `board mv FLK-n review` with a final note: what changed, how it was verified, and any shared files touched. The coordinator reviews and merges into `main`.
   - **`Cargo.lock`:** commit your lock changes. Never hand-merge a lock conflict: the coordinator takes `main`'s lock and lets `cargo build` regenerate it.
7. **Reviews.** After every two cards merge, the coordinator has Gemini 4 CARBON (`console-google/gemini-4-carbon-eap#high`) review what landed for bugs and for anything more complicated than it needs to be. Its fixes and cuts happen before the next cards start.
8. **Ask Ryan only through the board** (`board ask ... --card FLK-n`), only for taste calls, approvals or visual checks. Make ordinary design decisions yourself and record them in the design doc.
9. **After three failed attempts** at the same problem, stop, write the most likely wrong assumption on the card, and hand back.

## Models

Each card runs on one of three models, all at high reasoning, set on the board roster (`board agent ls`; agent names are `<model> · FLK-n`):

| Model | Ref | Gets |
|---|---|---|
| GPT-6 Astra | `openai/gpt-6-astra#high` | Performance-critical and low-level work: containers, codecs, hardware decode, GPU, player core, transport, transcoding, benchmarks |
| Opus 5.5 | `claude-agent/opus#high` | Design-heavy work: auth and storage, audio pipeline, Rune contributions, apps and UI |
| Sonnet 5.5 | `claude-agent/sonnet#high` | Well-specified plumbing: CI, scanner, service install, parsers |

## Commands

```sh
scripts/check.sh                                   # fmt, clippy -D warnings, cargo deny, all tests (needs ffmpeg and cargo-deny)
FLICKS_REQUIRE_ORACLES=1 scripts/check.sh          # as CI runs it: oracle tests fail instead of skipping
cargo test -p flicks-hevc                          # one crate
FLICKS_REQUIRE_ORACLES=1 cargo test -p flicks-h264 # fail instead of skip when ffmpeg is missing
cargo test -p flicks-core --release -- --ignored   # slow membership exploration
```

## Commits

One focused commit per verified piece. The message says what changed, why, and how it was verified (commands and numbers), and calls out removed or replaced behaviour. Never commit media files, credentials, or private logs and screenshots; generate fixtures in tests. Don't push, amend or rewrite history unless Ryan asks.
