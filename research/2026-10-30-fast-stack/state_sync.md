# State sync for a distributed media server (catalog, watch progress, membership/permissions)

Scope: how to replicate shared state across several home media servers and clients in Rust, as of 2026-10-30, extending Deadbolt's model. Sources were read on 2026-10-30. "(inference)" marks my own reasoning or back-of-envelope estimates, not a sourced fact. Data from before 2024 is marked [pre-2024].

**Recommendation in one paragraph (inference, built from the findings below):** Don't use one mechanism for everything. Split state into three streams and use the cheapest method for each one's write pattern:

- **Membership/grants:** keep Deadbolt's signed causal Add/Remove log. It is tiny, rarely written, never compacted, and always synced first.
- **Catalog:** each server is the *single writer* of its own catalog. Replicate it as a per-server sequence feed ("changes since seq N", one round trip, cost proportional to the changes). Use range-based set reconciliation (negentropy-style) as the fallback and anti-entropy path when a peer's cursor is older than the tombstone horizon, or for partial/library-scoped sync.
- **Watch progress:** make it a last-writer-wins (LWW) register per (user, item), written by that user's devices. Coalesce locally at Plex/Jellyfin's 10 s cadence, but replicate only on state changes plus a slow heartbeat. Keep it in a prunable per-device log, where a signed checkpoint lets older ops be dropped.

Extend Deadbolt's `HEADS → missing → RESULT` session with (a) multiple named streams, each with its own heads; (b) a "prune floor" per stream, so a peer behind it gets a snapshot or RBSR instead of ops; (c) direct eager pushes of new heads over already-open iroh connections. Use iroh-gossip only when a topic's swarm is larger than its active view (default 5).

---

## Q1. Op-log + heads exchange vs state-based CRDT registers vs range-based set reconciliation (RBSR) vs Merkle search trees vs Automerge/Loro/yrs: bandwidth and round trips to catch up 100k items with few changes

### Takeaway
Heads exchange is optimal (one round trip, bytes proportional to the number of logs plus the missing ops) *only while the full op history is kept*. With high-churn progress ops and weeks offline, it ships every intermediate tick. RBSR (negentropy, iroh-docs, Willow) reconciles *current state* in about 3 round trips for 100k–1M items, with bytes roughly proportional to the differences times log(n), and it needs no shared history. That makes it the right catch-up and repair tool after compaction. Automerge/Loro/yrs are built for concurrent rich-text editing and bring per-op metadata and history the catalog doesn't need. Merkle search trees work, but they force a rigid canonical structure.

### Cited Findings

**Deadbolt baseline (the model to extend)**
- One session runs on one bidirectional QUIC stream. Both sides concurrently send `HEADS(my heads)`, then `ENTRY* DONE` for `missing_for(peer heads)`, then the responder sends `RESULT`. Heads and entries are opaque bytes to the net layer. Membership entries are ordered before the data that depends on them. — [/Users/vogel/dev/deadbolt/crates/net/src/sync.rs](/Users/vogel/dev/deadbolt/crates/net/src/sync.rs)
- Data: "Each device appends to its own signed log. Entries reference the entries they were based on (causal), never wall-clock time… Sync = 'here are my heads, send me what I'm missing', over iroh… No compaction in v1… (Later: snapshots.)" — [/Users/vogel/dev/deadbolt/docs/design.md](/Users/vogel/dev/deadbolt/docs/design.md)
- The simulator checks convergence ("current members must agree on heads and items within twelve rounds"), removal confidentiality, durability and watch coverage, using seeded scheduling with no sockets or wall-clock time. Failures shrink to replayable traces. — [/Users/vogel/dev/deadbolt/docs/simulation.md](/Users/vogel/dev/deadbolt/docs/simulation.md)
- Startup: devices sync membership with every reachable peer before serving data, with a 30-second total budget (`2 * SYNC_TIMEOUT`). — [/Users/vogel/dev/deadbolt/docs/simulation.md](/Users/vogel/dev/deadbolt/docs/simulation.md)

**RBSR / negentropy**
- RBSR recursively partitions sorted sets and compares range fingerprints. "The number of rounds of the protocol only depends on the sizes of the data-sets being synced, and not on their number of differences." With 16-way splitting, rounds ≈ log₁₆(n)/2: about 3 round trips for 1 million items and about 4 for 1 billion. Many differences "distributed throughout" can make messages large and force fragmentation, which adds round trips. — [Hoyte, Log Periodic, Dec 2023](https://logperiodic.com/rbsr.html) [pre-2024, still the reference]
- Negentropy fingerprint: the sum mod 2²⁵⁶ of 32-byte IDs, concatenated with a varint count, SHA-256, truncated to 16 bytes ("tree-friendly… very efficient"). Range bounds use the shortest ID prefix that separates neighbours, and only a delta-encoded timestamp when timestamps differ. — [logperiodic.com/rbsr](https://logperiodic.com/rbsr.html)
- Negentropy "does not support… updating or changing a record while preserving its ID". Model updates as delete + insert. The ID is 32 bytes (usually a hash of the record) and the timestamp a u64; locality is best when similar timestamps are stored together. A frame-size limit caps message size at the cost of more round trips and some duplicate have/need reports. — [negentropy README](https://github.com/hoytech/negentropy)
- strfry/Nostr uses it "routinely… to synchronise data-sets of sizes in the 10s of millions of elements". — [logperiodic.com/rbsr](https://logperiodic.com/rbsr.html). NIP-77 standardised it for Nostr (first visible addition 2025-05). — [NIP-77 explainer](https://www.crays.org/nostr/nips/nip-77/)
- A Rust implementation exists (rust-negentropy, Yuki Kishimoto, "Stable", Vector storage). The C++ reference also has BTreeMem/BTreeLMDB and SubRange storage. AELMDB (an LMDB fork that keeps aggregate fingerprints in branch pages) reports a "4-10x" local efficiency gain. — [negentropy README](https://github.com/hoytech/negentropy); [Amparore, arXiv 2603.19820, Mar 2026](https://arxiv.org/abs/2603.19820)
- Meyer's RBSR paper (SRDS 2023) gives the generic design space, surveys cryptographically secure fingerprints, and "reduce[s] the time complexity of local computations by a logarithmic factor". — [arXiv 2212.13567](https://arxiv.org/abs/2212.13567); [SRDS 2023](https://www.computer.org/csdl/proceedings-article/srds/2023/291000a059/1UoH0OyouNa) [pre-2024]
- Partial sync: "clients could use this to sync just the latest week's worth of records" by sending ranges only for the parts they care about. — [logperiodic.com/rbsr](https://logperiodic.com/rbsr.html)

**Willow / iroh-docs / iroh-willow**
- Willow 3d-RBSR works over (subspace, path, time) ranges with commutative fingerprints. A peer either splits a mismatching range or sends its entry set with `want_response`. — [Willow 3d-RBSR spec](https://willowprotocol.org/specs/3d-range-based-set-reconciliation/index.html)
- iroh-docs 0.101.0 (crates.io 2026-06-15): entries keyed by (namespace, author, key) → (BLAKE3 hash, size, timestamp), synced with RBSR, stored in redb, and depends on iroh-blobs and iroh-gossip. "The namespace secret key is the write capability", so there is no per-device revocation. It is still pre-1.0 and two sync/timestamp bugs are open. iroh-willow is pinned to iroh 0.34 and dormant. — [Deadbolt research 02 (2026-10-15)](/Users/vogel/dev/deadbolt/docs/research/02-rust-sync-stacks.md); [iroh-docs README](https://docs.rs/crate/iroh-docs/latest/source/README.md)

**Set-reconciliation alternatives (IBLT / CPI / Bloom)**
- A 2023 benchmark found IBLT and CPI sync times "largely invariant to the size of the data sets", while Cuckoo-filter sync worsens as sets grow. CPI transfers "a nearly optimal amount of data per difference". Under a 2 s sync-time cap on a bad network, CPI used about 9x less bandwidth than IBLT. — [NSF PAR 10464278 (2023)](https://par.nsf.gov/servlets/purl/10464278) [pre-2024]
- Beelay planned RIBLT for doc-set reconciliation: "the server will be able to decode these symbols (because only one document has changed)" from "the first 5 symbols". — [Keyhive notebook 05 (2025-03)](https://www.inkandswitch.com/keyhive/notebook/05). However, Deadbolt's 2026-10-15 research found no RIBLT in current Subduction/Keyhive source. — [research 01](/Users/vogel/dev/deadbolt/docs/research/01-inkandswitch.md)
- Bloom-filter designs, like RBSR fingerprints, are open to censorship attacks with crafted inputs. — [logperiodic.com/rbsr](https://logperiodic.com/rbsr.html)

**Merkle search trees / prolly trees**
- MSTs "extend this idea to ordered CRDT states in open networks… by imposing a rigid, canonically derived tree representation", whereas RBSR lets each side use any tree or branching factor and still interoperate. — [arXiv 2603.19820 (2026)](https://arxiv.org/pdf/2603.19820); [logperiodic.com/rbsr "Stateless Sync"](https://logperiodic.com/rbsr.html)

**Automerge / Loro / yrs**
- Automerge's classic sync exchanges heads plus Bloom filters, "may require multiple iterations due to bloom filter false positives", and "requires having the entire commit graph in memory". Only the *initial* sync sends a compressed whole document; later syncs pay per-commit metadata overhead. — [Sedimentree design doc](https://raw.githubusercontent.com/automerge/beelay/main/docs/sedimentree.md)
- Automerge's compact binary encoding brings metadata overhead "to around 10% over the underlying data". — [Keyhive notebook 05](https://www.inkandswitch.com/keyhive/notebook/05)
- Automerge 3.0 (Aug 2025) keeps the compressed form at runtime and cut memory by "over 10x". One example went from 700 MB to 1.3 MB, and a document that "hadn't loaded after 17 hours" loaded "in 9 seconds". — [Automerge 3.0 blog](https://automerge.org/blog/automerge-3); [HN](https://news.ycombinator.com/item?id=44777086)
- Loro: a recent release note sped up a concurrent import on a shallow doc "with 100k maps in the shallow root" from about 240 ms to about 3.5 ms natively, and from about 2.8 s to about 6 ms in WASM. — [Loro releases](https://github.com/loro-dev/loro/releases). An open 2026-06 issue reports snapshot export becoming "very slow" on shallow docs after a small tail of edits. — [loro#992](https://github.com/loro-dev/loro/issues/992)

### Inferences
- **Catch-up after weeks offline, 100k-item catalog, about 50 changed items (estimates):**

| Approach | Round trips | Sync metadata | Notes |
|---|---|---|---|
| Deadbolt heads + full op log | 1 (heads both ways in parallel) + RESULT | about 40 B per log head × number of logs (50 logs ≈ 2 KB) | Then *every* op since the peer's head, including superseded ones. Cheap for the catalog (single writer, few ops). Bad for progress without compaction. |
| Single-writer seq feed ("since N") | 1 | 8–16 B cursor per source | Only works if the source keeps tombstones back to N. Otherwise fall back. |
| Negentropy RBSR over current state | about 3 (log₁₆ 100k / 2 ≈ 2.1, rounded up) | About d × levels × (16 B fingerprint + bound) per side: roughly 50 × 4 × ~20–40 B ≈ 4–8 KB worst case, less when changes cluster by timestamp | Needs no history. Works after compaction. CPU is a tree walk; use an aggregate-fingerprint B-tree to avoid rescans. |
| MST / prolly tree | About tree depth (3–4 for 100k with fan-out ~32) | Node hashes along changed paths | Both sides must build identical canonical trees. |
| Automerge/Loro doc of the catalog | Several (Bloom iterations) | Per-op metadata (about 10% compressed) plus full history in memory | Overkill for single-writer records. |

- Updates map naturally to RBSR when the record ID is `hash(record)` and the timestamp is the record's `updated` (or seq). An update is then delete + insert, as negentropy requires.
- For progress, an RBSR element would be `hash(user, item, hlc, position)`. Superseded values must be *removed* from the set (state-based LWW), so the set stays bounded by users × items touched.
- Heads exchange stays the best *steady-state* protocol: between connected peers that are nearly in sync it costs one tiny message. RBSR is the right *cold* path.

### Gaps
- I found no published head-to-head benchmark (bytes and round trips) of negentropy vs a heads/version-vector protocol vs an MST on the same dataset. The estimates in the table are mine.
- I did not retrieve negentropy's own per-message byte benchmarks (the Rust crate's numbers) or iroh-docs sync throughput numbers.
- No current (2025–2026) yrs performance data was gathered.

---

## Q2. Compaction / garbage collection for causal signed logs

### Takeaway
Every mature system bounds storage the same way: (1) overwrite semantics (LWW / prefix pruning), so superseded values can be deleted; plus (2) a signed checkpoint or snapshot that subsumes a log prefix. The cost is the same everywhere: a peer that is behind the checkpoint can't receive ops and must take a snapshot or reconcile by state.

### Cited Findings
- **p2panda:** `PruneFlag` is a built-in header extension "representing points in a log where all preceding operations can be deleted… garbage collection will automatically occur and eventually data will be removed network-wide". Choosing prune points (age, flagged CRDT changes) "is an application layer concern". Logs carry `backlink` plus `seq_num` ("sequence numbers are helpful to fastly detect forks and use the much faster and optimized diffing strategy when the local log is not forked"). Payloads are "Offchain" and can be dropped separately from signed headers. — [p2panda_core::prune 0.7.1](https://docs.rs/p2panda-core/0.7.1/p2panda_core/prune/index.html); [p2panda_core traits.rs](https://docs.rs/p2panda-core/0.7.1/src/p2panda_core/traits.rs.html)
- **Willow:** an entry is LWW per (subspace, path) by timestamp, then digest, then length. Prefix pruning means "a new entry at blogidea… deletes previous entries" under that prefix. Stores "form a join semi-lattice (also known as a state-based CRDT)". Timestamps are wall-clock (TAI µs recommended) and the authors acknowledge the limitations. — [Willow data model](https://willowprotocol.org/specs/data-model/index.html)
- **Sedimentree (Ink & Switch, Beelay → Subduction):** commits are chunked into strata at hash-derived boundaries. Two leading zeros in base 10 gives a level-1 boundary about every 100 commits, and level n about every 10ⁿ. Older history ends up in larger chunks. Sync = fetch the summary (stratum boundaries plus loose commits), then fetch or upload missing blobs. The summary omits interior checkpoints, because sending about 1% of 32-byte hashes "for large documents… would mean sending multiple megabytes". Peers with different histories agree on chunk contents because the traversal runs in reverse depth-first order from chunk ends. — [Sedimentree design](https://raw.githubusercontent.com/automerge/beelay/main/docs/sedimentree.md); [Keyhive notebook 05](https://www.inkandswitch.com/keyhive/notebook/05)
- Beelay is superseded by Subduction (subduction_core 0.19.0, 2026-10-07, with an iroh transport, "DO NOT use for production"). — [Deadbolt research 01](/Users/vogel/dev/deadbolt/docs/research/01-inkandswitch.md)
- **automerge-repo/samod storage compaction:** store each change as an incremental chunk. Once there are more than 10 new changes or more than 10 chunks, write a `doc.save()` snapshot keyed by a hash of the heads and delete the superseded chunks. — [research 01 citing samod on_disk_state.rs](/Users/vogel/dev/deadbolt/docs/research/01-inkandswitch.md)
- **Loro shallow snapshots:** "like Git's shallow clone", "typically 70-90% smaller". "Peers can only sync if they have versions after the shallow snapshot point." Best practice: "Coordinate before trimming: Ensure all peers synchronized." — [Loro shallow snapshots](https://loro.dev/docs/concepts/shallow_snapshots)
- **Biscuit/UCAN revocation lists** are bounded by expiry: "All tokens should come with an expiration date, to prevent the revocation list from growing indefinitely" ([Biscuit](https://www.biscuitsec.org/docs/guides/revocation)). "Revocations MAY be evicted once the UCAN that they reference expires" ([UCAN revocation v1.0.0-rc.1](https://github.com/ucan-wg/revocation/blob/main/README.md)).

### Inferences
- **Proposed Deadbolt extension: "checkpoint" entries.**
  1. A device's log may contain a signed `Checkpoint { stream, covers: heads, state_root, prune_below: seq }`.
  2. Peers may delete that device's own entries below `prune_below` once the checkpoint is durable. This is the p2panda PruneFlag pattern, but scoped to one's own log, so no cross-device agreement is needed.
  3. In `missing_for`, if the peer's head for that log is below the floor, send the checkpoint's state (or run RBSR over the stream's state set) instead of ops.
- Never prune the membership log. Its causal structure carries the seniority and removal rules in design.md, and it is small.
- **Progress:** an LWW register keyed by (user, item) is the whole state. Old ops are pure garbage once a newer op for the same key exists, which is Willow's overwrite semantics. Storage is bounded by (users × items ever touched) × ~60–100 B (inference). For example, 10 users × 5k touched items ≈ 50k registers ≈ 3–5 MB.
- **LWW clock:** Willow shows wall-clock LWW is workable, but a skewed device clock can win forever. Use a hybrid logical clock (max of the local wall clock and seen timestamps, plus a counter), with the device ID as tie-breaker. Deadbolt's "edit replaces only if based on previous value" rule can be dropped for progress, because the newest write is meant to win.
- **Catalog:** with a single writer per server, compaction is just "keep the latest record per item plus tombstones for X days". Peers behind the tombstone horizon fall back to RBSR, which detects deletions by absence.
- Snapshot cadence borrowed from samod: checkpoint after about 10 chunks or more than N ops. For progress, something like daily or every 1,000 ops per device (inference).

### Gaps
- Keyhive's own content GC policy beyond sedimentree was not found. Its issue #217 asks for an eviction / finite-retention API ([research 01](/Users/vogel/dev/deadbolt/docs/research/01-inkandswitch.md)).
- I found no published storage-growth numbers for p2panda pruning in practice.

---

## Q3. Write amplification for watch progress: how often Plex and Jellyfin report progress, and how to coalesce

### Takeaway
The industry cadence is one progress report about every 10 s (20 s on cellular for Plex), plus an immediate report on every state change (pause, seek, stop). Servers interpolate between reports. Replicating every 10 s tick to every peer is unnecessary. Persist ticks locally and replicate only state changes plus a slow heartbeat.

### Cited Findings
- **Plex Media Server API, `/:/timeline`:** it "must be hit whenever the play state changes, or in the absence of a play state change, in a regular fashion (generally this means every 10 seconds on a LAN/WAN, and every 20 seconds over cellular)". Parameters include `state` (stopped/buffering/playing/paused), `time`, `duration`, and, for offline sync clients, `updated` ("the time at which the playback state was last updated") and `offline=1` ("a timeline is being synced from being offline"). — [Plex developer docs](https://developer.plex.tv/pms)
- Plex already has an offline-replay concept: a sync client replays timelines with their original `updated` time. That is effectively an LWW timestamp. — [Plex developer docs](https://developer.plex.tv/pms)
- **Plex remote-control (player → controller):** controllers re-subscribe every 30 s, and subscriptions time out after 90 s. Players "MUST send timeline request to controllers every time the state changes, or at a minimum, every time the play offset increments by a second". This is a 1 Hz *ephemeral* UI stream, separate from persisted progress. — [plex-media-player wiki](https://github-wiki-see.page/m/plexinc/plex-media-player/wiki/Remote-control-API) [older, undated]
- PMS websocket `PlaySessionStateNotification` appeared at about 10 s intervals to a 2019 user. — [Plex forum 2019](https://forums.plex.tv/t/is-it-possible-to-change-the-websocket-notifications-interval/418457) [pre-2024]
- **Jellyfin web (current master):** `startPlaybackProgressTimer` uses `setInterval(onPlayerProgressInterval, 10000)`, and the player's `timeupdate`, `pause`, `unpause`, `volumechange` and other events also call `sendProgressUpdate` → `reportPlaybackProgress` (POST `/Sessions/Playing/Progress`). — [jellyfin-web playbackmanager.js](https://raw.githubusercontent.com/jellyfin/jellyfin-web/master/src/components/playback/playbackmanager.js)
- Third-party Jellyfin SDK docs: progress heartbeat "Recommended every 10 s". — [dart_jellyfin](https://pub.dev/documentation/dart_jellyfin/latest/dart_jellyfin/JellyfinPlaybackApi-class.html)
- **Emby (Jellyfin's ancestor):** report progress "Automatically every 10 seconds" and "Immediately following any user interaction". "The server will automatically increment playback progress every second, so it is not necessary to automatically report more often than at 10 second intervals." Progress can also travel over the websocket. — [Emby Playback Check-ins](https://dev.emby.media/doc/restapi/Playback-Check-ins.html)
- **Jellyfin server thresholds** (configurable): `MinResumePct`, `MaxResumePct`, `MinResumeDurationSeconds`, audiobook variants. Below the minimum, playstate isn't saved; past the max, the item is marked watched and position resets. — [Jellyfin API](https://api.jellyfin.org/). Streamyfin mirrors these defaults as 5% minimum and 90% played. — [streamyfin usePlaybackManager.ts](https://raw.githubusercontent.com/streamyfin/streamyfin/develop/hooks/usePlaybackManager.ts)

### Inferences
- **Volume without coalescing:** 3 h/day of playback at 10 s = 1,080 ops/day per active viewer. At about 100–150 B per signed op (64 B Ed25519 signature + IDs + HLC + position), that is about 110–160 KB/day per viewer, or about 1 MB per viewer per week offline. Heads catch-up would replay all of it.
- **Proposed coalescing (three tiers):**
  1. **Ephemeral (not persisted, not signed):** about 1 Hz "now playing" to watchers over an open connection or a gossip topic (Plex controller model).
  2. **Local durable:** write the position to the local DB every 10 s (crash safety, Plex/Emby cadence).
  3. **Replicated signed op:** only on start, pause, stop, seek of more than 30 s, app background, crossing the watched threshold (≥ 90%), and a heartbeat about every 2–5 min while playing. That gives about 40–100 ops/day per viewer (10–25x fewer). With per-key LWW compaction, steady-state storage stays at one register per (user, item).
- Skip replicating progress below the min-resume threshold (5% / MinResumeDurationSeconds). The value would be discarded anyway.
- Batch-sign: one signature over a batch of register updates per flush (like Deadbolt's "as few entries as fit (~1 MiB each)" import batching), rather than one signature per tick.
- Keep "played" (watched flag, play count) as a separate register or a grow-only counter, so a stale low position can't un-watch an item (inference).

### Gaps
- Whether Jellyfin's HTML video player throttles `timeupdate` before it reaches `sendProgressUpdate` was not verified. If it doesn't, jellyfin-web may report more often than every 10 s during playback.
- Whether PMS/Jellyfin persist to the DB on every progress report or only on stop was not verified from server source.
- No published data on Plex/Jellyfin DB write load from progress reports was found.

---

## Q4. Capability-based sharing for friends: UCAN, Keyhive, Meadowcap, Macaroons/Biscuit (verification cost, revocation)

### Takeaway
All of these chained-signature capability systems cost about one signature check per delegation hop. Revocation is always an extra, monotonic, eventually consistent list, bounded only by expiry. For per-library read grants to a few friends, where the *granting household's servers are the only resource holders*, the cheapest correct design is Deadbolt's: a signed `Grant`/`Revoke` op in the owner household's membership log, checked by an O(1) gate on accept and dial at the serving server. Add short-lived capabilities only if friends' servers must re-serve (cache) content to third parties.

### Cited Findings
- **Meadowcap (Willow):** capabilities answer "to whom… read or write… for which Entries… is it valid". They can be delegated, and restricted by subspace, path prefix and/or timestamp range. Verification is recursive: check the last delegation's signature, then verify the previous capability, so cost is proportional to the delegation depth. — [Meadowcap spec](https://willowprotocol.org/specs/meadowcap/index.html)
- **UCAN revocation (v1.0.0-rc.1):**
  - "Revocations MUST be immutable and irreversible… a monotonically-growing set… highly amenable to caching and gossip."
  - The resource-controlling agent "MUST maintain a cache of Revocations", and each delegation CID in a chain is checked against it.
  - "Resources with a single source of truth SHOULD… maintain a revocation store at the same physical location as the resource."
  - Revocations may be evicted after the referenced UCAN expires.
  - "Proactive expiry… SHOULD be preferred."
  - — [UCAN revocation spec](https://github.com/ucan-wg/revocation/blob/main/README.md)
- **Biscuit:** revocation IDs are derived per block from the token's structure, so revoking a parent revokes derived (attenuated) tokens. It recommends distributing lists by periodic download, diffs keyed by an incrementing ID, or queues. "All tokens should come with an expiration date." — [Biscuit revocation guide](https://www.biscuitsec.org/docs/guides/revocation)
- **Keyhive:**
  - A revocation records the document heads it was issued after.
  - "Keyhive cannot distinguish a genuinely old operation that arrives late from a back-dated one."
  - It is pre-alpha and unaudited, and Keyline (the successor authority model) was still being wired in as of 2026-10-15.
  - — [Deadbolt research 01, citing the Keyhive threat model T5](/Users/vogel/dev/deadbolt/docs/research/01-inkandswitch.md)
- **iroh-docs:** the write capability is the namespace secret, so it can't be revoked per device. — [research 02](/Users/vogel/dev/deadbolt/docs/research/02-rust-sync-stacks.md)
- **p2panda-auth:** a "convergent access control CRDT" with per-member permissions. p2panda-spaces combines it with group encryption, but it is unaudited and group control messages are unencrypted. — [research 02](/Users/vogel/dev/deadbolt/docs/research/02-rust-sync-stacks.md); [p2panda access-control notes (2025-08)](https://p2panda.org/2025/08/27/notes-convergent-access-control-crdt.html)
- **Deadbolt's model:** a single "is this EndpointId in the current member set?" check on accept *and* dial; existing connections close immediately when a `Remove` arrives; membership syncs first on startup. Back-dated removal races are a documented, open limitation. — [design.md](/Users/vogel/dev/deadbolt/docs/design.md)

### Inferences
- **Proposed model for friends (two layers):**
  1. **Household membership** (devices and servers of one household, full rights): Deadbolt's Add/Remove log, unchanged.
  2. **Grants:** the household admin appends `Grant { friend_user_key, library_id, rights: read, expires }` / `RevokeGrant` to the same causal log. Friends' devices never receive the household log. They get only (a) the catalog stream for granted libraries, and (b) their own progress stream back.
  - **Enforcement:** the serving server checks `(peer, library) ∈ grants` per request. This is an in-memory hash lookup (ns-scale), versus one Ed25519 verify per hop for a presented capability chain (generally cited as tens of µs; not verified here).
  - **Revocation:** takes effect as soon as each household server learns it, which is fast because household servers are few and always online. This is UCAN's "store at the resource" locality.
- Use Meadowcap/UCAN/Biscuit-style bearer capabilities only if friends' devices must *prove* access to a *third* server that doesn't have the grant log (for example, a friend's own server re-serving a cached copy). Give them short expiries (days) and renew automatically over the grant connection, so revocation lists stay empty or tiny.
- **Per-library scoping maps onto Willow/Meadowcap areas:** namespace = household, subspace = server, path prefix = `/library/<id>/`. This is also a natural key prefix for RBSR partial sync of only the granted libraries.
- Progress written by friends belongs to the friend. Store it in the friend's own stream and *share* it with the host household (needed for "continue watching" on the host's servers), so friends never need write capability on the host's catalog.

### Gaps
- I did not retrieve Meadowcap's explicit statement on revocation. My understanding is that it relies on time-restricted capabilities rather than revocation lists, but this is unverified in this pass.
- No measured verification-latency numbers for Biscuit (Datalog evaluation), UCAN chains or Meadowcap were retrieved.
- Macaroons (HMAC chains: cheap to verify but they need the root secret at the verifier) were not researched with a primary source.

---

## Q5. Gossip/fanout among N nodes: iroh-gossip (HyParView/PlumTree), push vs pull

### Takeaway
For one household (about 2–10 servers and clients) plus a few friends, every node can keep a direct iroh connection to every peer that matters. Eager-push "new heads" or small register batches over those connections, and pull (heads exchange or RBSR) on reconnect. iroh-gossip's default active view is 5 peers, so for swarms of 6 or fewer it degenerates to near-full mesh while adding a topic membership protocol. It pays off only for wide, ephemeral fanout, such as a 1 Hz "now playing" stream to many watchers or federation across many households.

### Cited Findings
- **iroh-gossip:** HyParView membership plus PlumTree broadcast per topic. "In the default configuration, the active view has a size of 5 and the passive view a size of 30."
  - Broadcasts are pushed in full to *eager* peers, and only the message hash ("IHave") is pushed to *lazy* peers. A lazy peer can request the content after a timeout (Graft) and is then promoted to eager, which "self-optimizes the messaging graph by latency".
  - "Joining multiple topics increases the number of open connections to peers and the size of the local routing table."
  - — [iroh-gossip 0.101.0 proto docs](https://docs.rs/iroh-gossip/0.101.0/iroh_gossip/proto/index.html)
- **PlumTree config** (as re-exported by p2panda-net): `graft_timeout_1`, `graft_timeout_2` (the second "in the order of an average round trip time"), `dispatch_timeout` (IHave batching), `optimization_threshold`, `message_cache_retention`. — [p2panda-net PlumTreeConfig](https://docs.rs/p2panda-net/0.7.1/p2panda_net/gossip/type.PlumTreeConfig.html)
- **PlumTree paper:** "Eager push strategies produce more redundant traffic but they also achieve lower latency than lazy push or pull strategies, as they require at least an extra round trip". Experiments used active view size 5 and fanout 4. — [Leitão et al., SRDS 2007](https://www.joaoleitao.org/pdf/srds07-leitao.pdf) [pre-2024]
- iroh docs: use gossip for "Lightweight broadcast where eventual delivery is acceptable", where membership is dynamic, and when no single point of failure is wanted. Gossip "increases traffic" through redundancy. — [iroh gossip docs](https://docs.iroh.computer/connecting/gossip)
- Deadbolt research 02 concluded that with fewer than about 10 personal devices "direct pushes are simpler" (inference there). iroh-gossip 0.101 is active but pre-1.0. — [research 02](/Users/vogel/dev/deadbolt/docs/research/02-rust-sync-stacks.md)
- RBSR can skip "the most recent few seconds to avoid redundant double-downloads if records are also being streamed in real-time". This is the documented push+pull combination. — [logperiodic.com/rbsr](https://logperiodic.com/rbsr.html)

### Inferences
- **Push vs pull policy:**
  - **Push (eager, small):** new progress-register batches and catalog seq bumps to currently connected household peers. Use a tiny `NOTIFY(stream, new_head)` frame, and let the receiver pull if it was behind.
  - **Pull:** a heads exchange on every (re)connect and startup (membership first, as in Deadbolt). If a stream's heads are behind the peer's prune floor, use RBSR or a snapshot.
  - **Periodic anti-entropy:** RBSR every few hours between always-on servers, to catch bugs and missed pushes cheaply (about 3 round trips and a few hundred bytes when in sync).
- **Fanout:** with servers as hubs, clients (phones, TVs) connect to one or two household servers rather than to every device. That turns O(N²) device sync into O(N) client-to-server links plus a small server mesh. Servers are the always-on, durable replicas, like Deadbolt's future mailbox but with plaintext.
- iroh-gossip fits the ephemeral 1 Hz "now playing" topic and large federations. Avoid it for durable state, because PlumTree's message cache retention is bounded and delivery is best-effort.
- **Simulator:** the Deadbolt deterministic simulator can extend to these properties:
  - convergence of LWW registers after a prune;
  - a peer behind the prune floor converges via snapshot or RBSR;
  - grant revocation stops catalog and progress flow to the friend within one round;
  - catch-up bytes stay bounded after a simulated 4-week partition.

### Gaps
- No measured iroh-gossip overhead (bytes per message, connections per topic) for small swarms was found.
- No published guidance from n0 on gossip vs direct push thresholds was found.
