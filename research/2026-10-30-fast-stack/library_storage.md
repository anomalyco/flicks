# Library pipeline: scan, identify, store and query a large personal media library (late 2026)

Scope: the runtime cost of each stage for a single small Rust binary serving 10k–100k files (tens of TB) on HDD/NAS, compared with Plex, Jellyfin, Emby, Navidrome and Kodi. Dates are given where known; older data is marked **[old]**.

Note on method: the Parallel search tool hit a rate limit partway through, so the later checks used the built-in web search and fetch. Some pages (Navidrome scanning docs, the BLAKE3 PDF) could not be fetched directly, so their numbers come from search excerpts of primary sources such as GitHub code, datasheets and READMEs.

---

## 1. Filesystem scanning and change detection

### Takeaway
Use OS events where they work and scheduled metadata-only rescans everywhere else. Events cover local disks: inotify watches one directory each, about 1 KB of kernel memory per watch; FSEvents and ReadDirectoryChangesW are recursive natively. NFS/SMB mounts give no events, so they need the rescan. Make the rescan cheap with per-folder hashes of the entry list, names, sizes and mtimes (Navidrome's approach), so an unchanged library costs only `readdir` and `stat` calls and opens no files. Jellyfin's and Plex's watchers often escalate to whole-library scans with ffprobe on every file, and that is where their cost comes from.

### Cited Findings
**Kernel and OS watcher limits**
- The inotify default `max_user_watches` was 8192 from 2005. Kernel 5.11 changed the default to scale with RAM: about 1% of addressable memory, clamped to 8,192–1,048,576. One watch (inotify_inode_mark plus 2 VFS inodes) costs "a bit over 1 kbytes (1284 bytes with my x86-64 config)", so machines with ≥128 GB RAM get the 1,048,576 maximum. — [LKML patch, Waiman Long, Nov 2020](https://lkml.rescloud.iu.edu/2011.1/00435.html); 5.11 confirmation — [Stack Overflow](https://stackoverflow.com/questions/535768/what-is-a-reasonable-amount-of-inotify-watches-with-linux)
- inotify limits apply per user, not per application, so every program a user runs that watches files competes for the same pool (`max_user_instances`, `max_user_watches`, `max_queued_events`). — [watchexec docs](https://watchexec.github.io/docs/inotify-limits.html)
- fanotify `FAN_MARK_FILESYSTEM` (Linux 4.20+) watches a whole filesystem without per-directory marks, but it requires `CAP_SYS_ADMIN`. Without that capability, fanotify allows only inode marks with file-handle (FID) reporting. Plain fanotify directory monitoring is not recursive, and mount/filesystem marks are the race-free way to watch a whole tree. — [fanotify_mark(2)](https://www.man7.org/linux/man-pages/man2/fanotify_mark.2.html), [fanotify(7)](https://man7.org/linux/man-pages/man7/fanotify.7.html), [kernel fanotify_user.c](https://github.com/torvalds/linux/blob/master/fs/notify/fanotify/fanotify_user.c)
- "Fanotify reports only events that a user-space program triggers through the filesystem API. As a result, it does not catch remote events that occur on network filesystems." — [fanotify(7)](https://man7.org/linux/man-pages/man7/fanotify.7.html)
- Windows `ReadDirectoryChangesW` supports subtree (recursive) watching. On a network share it fails with `ERROR_INVALID_PARAMETER` if the buffer is larger than 64 KB. When the buffer overflows, all buffered events are discarded and the call returns `ERROR_NOTIFY_ENUM_DIR`, so the caller must rescan. libuv hard-codes 64 KB for recursive watches. — [libuv fs-event.c](https://github.com/libuv/libuv/blob/v1.x/src/win/fs-event.c), [Stack Overflow / MSDN quote](https://stackoverflow.com/questions/33119881/changing-internalbuffersize-on-filesystemwatcher-for-a-network-path), [watchdog issue](https://github.com/gorakhargosh/watchdog/issues/264)
- The old .NET FileSystemWatcher default buffer of 4 KB covers about 80 file changes (16 bytes plus the UTF-16 name per event). The kernel copy of that buffer comes from non-paged pool memory. — [Microsoft docs](https://learn.microsoft.com/en-us/previous-versions/ded0dc5s(v=vs.100)?redirectedfrom=MSDN) **[old]**
- macOS FSEvents is recursive by design. fswatch's author reports "no performance degradation" watching a 500 GB filesystem over long periods. — [fswatch manual](https://emcrisostomo.github.io/fswatch/doc/1.17.1/fswatch.html/Monitors.html)

**Rust `notify` crate**
- Current versions are 8.x (8.2.0 released 2025-08-03; MSRV 1.88). The docs warn: "Network mounted filesystems like NFS may not emit any events"; the workaround is `PollWatcher`. In recursive watches every file and folder counts toward the limit. Version 8.2.0 added a notice when `max_user_watches` is reached. — [docs.rs notify](https://docs.rs/notify/latest/notify), [CHANGELOG](https://github.com/notify-rs/notify/blob/main/notify/CHANGELOG.md), [repo](https://github.com/notify-rs/notify)

**Parallel directory walking in Rust**
- `jwalk` (rayon-parallel, sorted streaming) is marked "Unmaintained … Use `dua-core` instead". — [Byron/jwalk](https://github.com/Byron/jwalk)
- An early jwalk benchmark on APFS: walkdir 161 ms, walkdir sorted with metadata 426 ms, jwalk 64 ms. The `ignore` crate's `WalkParallel` also beat serial walking. Another commenter found the walk I/O-bound: CPU use "jumped from 1% to 70%" while "overall I/O-bound speed did not change at all". — [users.rust-lang.org 2019](https://users.rust-lang.org/t/feedback-on-crate-for-parallel-recursive-directory-walk/25001) **[old]**
- `walkdir` is single-threaded, and its README says its speed is comparable to `find`. — [crates.io walkdir](https://crates.io/crates/walkdir)

**How the incumbents detect changes, and what it costs**
- Plex has hidden settings `FSEventLibraryUpdatesEnabled` ("scan my library automatically") and `FSEventLibraryPartialScanEnabled` ("only scan the folder that changed"), both off by default. Plex's own label for `watchMusicSections` warns that "Linux systems limit the maximum number of watched directories; this may cause problems with large music libraries." — [WebTools-NG settings list](https://webtools-ng.github.io/manual/library-list.html)
- In Jellyfin 10.11, adding one new file (even a subtitle) in a movie folder triggers a scan of the whole library root. The cause is that movie folders have no DB entries, so `FileRefresher` walks up to the library root. Jellyfin then re-runs ffprobe with `-analyzeduration 200M -probesize 1G` across the library. It was still reported on 10.11.6 in Feb 2026, and the `/Library/Media/Updated` API path causes 10–15 minute delays. — [jellyfin#13401](https://github.com/jellyfin/jellyfin/issues/13401)
- Jellyfin 10.11.0 full library scans regressed from 2 min 30 s on 10.10.7 to 29 min 53 s for one user, with `database table is locked` errors. — [jellyfin#15070](https://github.com/jellyfin/jellyfin/issues/15070) (Oct 2025)
- On Jellyfin v12-rc, "query congestion during library scan… happens on every library scan even without any changes"; on 10.10 those no-change scans finished in under a minute. — [jellyfin#17192](https://github.com/jellyfin/jellyfin/issues/17192) (Jun 2026)
- Jellyfin once launched 1,105 concurrent ffprobe processes on Windows, pinning an external HDD at 100% (fixed in 10.11). Another user saw more than 250 ffprobes on 8 spinning disks use nearly 50 GB of RAM before crashing. — [jellyfin#14497](https://github.com/jellyfin/jellyfin/issues/14497), [jellyfin#12203](https://github.com/jellyfin/jellyfin/issues/12203) (2024–25)
- Jellyfin's docs say that for networked storage you should use NFSv4 because it has built-in locking, and that cloud-storage users should disable image extraction because it downloads the entire file. — [Jellyfin storage docs](https://jellyfin.org/docs/general/administration/storage)
- **Navidrome's model (the lightweight reference):** since PR #4220 (June 2025, shipped in v0.57), each folder row stores an MD5 over the folder mtime, playlist and subfolder counts, image timestamp, and the sorted file names with each file's size and mtime. A quick scan rescans a folder only if this hash changed. — [navidrome PR #4220](https://github.com/navidrome/navidrome/pull/4220), [folder_entry.go](https://github.com/navidrome/navidrome/blob/220019a9/scanner/folder_entry.go)
- Navidrome's v0.55 "BFR" scanner uses `fsnotify` to "trigger scans only on the modified folders", supports resumable full scans, and handles moves and renames. — [navidrome PR #2709](https://github.com/navidrome/navidrome/pull/2709), [v0.55.0 release](https://github.com/navidrome/navidrome/releases/tag/v0.55.0)
- A later Navidrome bug-fix release targets slow storage (USB, remote drives) that saw floods of `database is locked` errors after 0.64.0. In the fix, the artwork worker pauses during scans, `ANALYZE` runs one index at a time, and Synology `@eaDir` folders are ignored. — [navidrome releases](https://github.com/navidrome/navidrome/releases)

### Inferences
- inotify watches directories, and a watch on a directory reports events for its children. A movie/TV library typically has one folder per title or season, so 100k files means roughly 10k–30k watches, about 13–40 MB of kernel memory at ~1.3 KB each. That fits under the 5.11+ defaults on machines with a few GB of RAM, but you should check `max_user_watches` and fail over to polling cleanly.
- On NAS-backed mounts (NFS/SMB), and when a NAS box modifies files locally, the server sees no events. The only reliable design is a periodic metadata walk (`readdir` and `stat`) compared against stored per-directory hashes. Event watchers then act only as a latency optimization.
- The fix for the Jellyfin and Plex failure mode is to resolve each change event to the smallest affected directory. Never escalate to the library root, and never re-probe a file whose (size, mtime, inode) is unchanged.
- Parallel walking helps on SSD and NFS (network latency hides behind concurrency) but barely helps on a single HDD, where seek time dominates. Parallelize per physical disk or mount, not per CPU core.

### Gaps
- No 2024–2026 head-to-head benchmark of jwalk vs `ignore::WalkParallel` vs walkdir vs `dua-core` on HDD or NFS was found. The dirstride crate page has numbers, but its excerpt was too garbled to cite.
- No measurements were found of a metadata-only rescan (`stat` of 100k files) over NFS or SMB.
- No public documentation was found for Plex's internal scanner algorithm (whether it uses mtimes or hashes).

---

## 2. File identity: cheap fingerprints vs full BLAKE3

### Takeaway
Identify files by (size, OSHash or a similar sampled-chunk hash), which reads only 128 KB per file regardless of size, and treat it as a dedupe hint across servers, not a security boundary. Compute full BLAKE3 lazily, only when a file is actually transferred or verified. Hashing a 50 TB library is bounded by HDD speed (about 180–285 MB/s per drive), not by CPU: BLAKE3 runs at 3–13 GB/s per core.

### Cited Findings
- **OSHash** = file_size + 64-bit wrapping sum of the first 64 KB + sum of the last 64 KB, read as little-endian uint64s. Exactly 128 KB is read regardless of file size, so "hashing a 50 GB file takes the same time as a 200 KB file". The minimum file size is 128 KB. — [opensubtitles/oshash README](https://github.com/opensubtitles/oshash), [reference site](https://opensubtitles.github.io/oshash)
- The OSHash reference site warns of trivial collisions (same size and same first and last 64 KB) and says to "Use SHA-256 or BLAKE3" where collision resistance matters. — [opensubtitles.github.io/oshash](https://opensubtitles.github.io/oshash)
- BLAKE3 single-thread, 16 KiB input, AWS c5.metal (Cascade Lake-SP, AVX-512): 6,866 MiB/s, vs SHA-256 484, SHA-1 1,027 and BLAKE2b 1,312 MiB/s. — [BLAKE3-specs bar_chart.py](https://github.com/BLAKE3-team/BLAKE3-specs/blob/master/benchmarks/bar_chart.py) **[old, 2019 hardware]**
- The paper's claim: peak single-thread throughput is 12× SHA-256 and 4× BLAKE2b on Cascade Lake-SP, and it scales further with threads. — [BLAKE3 paper](https://raw.githubusercontent.com/BLAKE3-team/BLAKE3-specs/master/blake3.pdf)
- OpenZFS kernel benchmarks at 1 MiB blocks, in MiB/s: Ryzen 7 5800X avx2 about 4,850, sse41 about 2,500, sha256-sha_ni about 2,280. i3-1005G1 avx512 about 5,760–6,100, avx2 about 3,300. — [OpenZFS PR #12918](https://github.com/openzfs/zfs/pull/12918), [BLAKE3#207](https://github.com/BLAKE3-team/BLAKE3/issues/207) **[2022]**
- About 13.2 GB/s single-core on Zen 5 (EPYC 4245P, AVX-512) for 1 MB inputs, vs SHA-256 at about 2.37 GB/s. — [daily.dev summary, Aug 2026](https://daily.dev/posts/blake3-at-13gb-s-on-zen-5-pwusg1h2t) (secondary source)
- Apple Silicon: the Rust blake3 crate on an M5 Pro runs about 2.40 GB/s on 1 core and about 38.6 GB/s on all cores for 1 GiB in memory. — [turboblake benchmarks](https://gunta.github.io/BLAKE3-bend/benchmarks/) (third-party, 2026). On an M2 Max, single-thread b3sum is about 12% slower than OpenSSL SHA-256, which uses the ARMv8 SHA instructions. — [BLAKE3#315](https://github.com/BLAKE3-team/BLAKE3/issues/315) (2024 update)
- HDD sequential throughput (vendor maximum, outer diameter): WD Red Plus 180–260 MB/s depending on model; IronWolf Pro 18–32 TB 255–285 MB/s; WD Red Pro up to 287 MB/s. IronWolf Pro average rotational latency is 4.16 ms. — [WD Red Plus brief](https://www.westerndigital.com/content/dam/doc-library/en_us/assets/public/western-digital/product/internal-drives/wd-red-plus-hdd/product-brief-western-digital-wd-red-plus-hdd.pdf), [IronWolf Pro 32TB datasheet](https://www.asipartner.com/marketing/Seagate26/STX-disiti-32tb-LP/resources/ironwolf-pro-32-tb-DS2129-7-2510US-en_US.pdf), [WD Red Pro datasheet](https://documents.westerndigital.com/content/dam/doc-library/en_us/assets/public/western-digital/product/internal-drives/wd-red-pro-hdd/data-sheet-wd-red-pro-hdd.pdf), [IronWolf Pro 2016 datasheet](https://www.seagate.com/files/www-content/product-content/nas-fam/nas-hdd/_shared/docs/1914.1-1611us-ironwolfpro-hires-ds.pdf)
- A full read over gigabit SMB measured 96 MB/s: 36 GB in 373 s. The same tool reading only headers plus a title lookup did the same 13 files in 2.4 s. — [FileVerdict scanning-speed docs](https://fileverdict.com/docs/guides/scanning-speed/)
- **iroh-blobs / bao outboards:** standard bao keeps hashes down to 1 KiB chunks, two 32-byte hashes per chunk, so the outboard is 1/16 of the data size: "a 32 gb disk image would require a 2gb outboard". — [iroh#741](https://github.com/n0-computer/iroh/issues/741) (2023). iroh-blobs instead stores the outboard only down to 16 KiB "chunk groups", which is ~16× smaller. Data ≤16 KiB has a zero-size outboard, and the outboard is stored separately so data can be used as-is in place. — [iroh blob-store design](https://www.iroh.computer/blog/blob-store-design-challenges), [blake3-optimized README](https://github.com/lamb356/blake3-optimized)
- iroh-blobs range requests have 16 KiB granularity, and worst-case overhead is about two chunk groups per range. — [iroh-blobs protocol docs](https://docs.rs/iroh-blobs/latest/iroh_blobs/protocol/index.html)

### Inferences
- Full-hash cost at 250 MB/s per HDD: a 40 GB remux takes about 160 s. 50 TB on one spindle takes about 200,000 s (≈ 2.3 days). Eight disks hashed in parallel take about 7 hours. CPU is not the limit: one AVX2 core at ≈3–5 GB/s can keep up with more than 10 HDDs. Full hashing at initial scan is therefore unacceptable for "fast initial scan". Do it lazily, at low I/O priority, or only for files being shared.
- OSHash cost on HDD is about 2 seeks (head and tail) plus 128 KB, so roughly 15–30 ms per file. 100k files on one disk ≈ 25–50 min if done serially, much less spread across disks. Reading only on new or changed files (by size and mtime) makes rescans near-free.
- Cross-server dedupe: (size, OSHash) is a cheap, widely deployed key (it is also OpenSubtitles' lookup key). Use it to find candidate duplicates, then confirm with BLAKE3 only when bytes will move between nodes. TMDB/TVDB IDs plus edition, resolution and codec can group "the same movie" even when the encodes differ.
- iroh-blobs outboard size: 64 bytes per 16 KiB group is about 0.39% of the data (computed from the 1/16 figure ÷ 16). That is about 156 MB for a 40 GB file and about 195 GB for 50 TB. Do not precompute outboards for the whole library; build them on demand and cache them.

### Gaps
- No published benchmark of OSHash or other sampled-hash fingerprinting on HDD/NAS at 100k-file scale.
- The exact iroh-blobs outboard ratio is derived above, not quoted from iroh docs.
- No collision-rate data for OSHash on real media libraries.

---

## 3. Media probing: ffprobe vs in-process Rust demuxers

### Takeaway
Spawning an ffprobe per file costs about 100 ms per file even in its light mode. Jellyfin's default flags (`-analyzeduration 200M -probesize 1G`) can read far more data than that. An in-process header-only parse (Matroska EBML segment info and tracks, MP4 `moov`) reads kilobytes and needs no process spawn. Pure-Rust parsers exist (symphonia-format-mkv/isomp4, matroska-demuxer, mp4, mp4-atom), but no benchmarks were found for them.

### Cited Findings
- A ffprobe header check (format and stream entries only) costs "~100ms per file". A full decode of a 4K remux takes 10–15 min and reads 50+ GB. — [McGarrah blog, Aug 2026](https://mcgarrah.org/jellyfin-media-integrity-architecture-design/)
- Jellyfin invokes `ffprobe -analyzeduration 200M -probesize 1G … -show_streams -show_chapters -show_format` per file. — [jellyfin#13401 logs](https://github.com/jellyfin/jellyfin/issues/13401)
- One user got about 4.7 files/s (30,160 files in 105 min) running serial ffprobe from Perl. The suggested fix was parallel ffprobe via `xargs -P`. — [ffmpeg-user 2020](https://ffmpeg.org/pipermail/ffmpeg-user/2020-January/046468.html) **[old]**
- Jellyfin PR #4242 made concurrent ffprobe the default (concurrency = CPU cores). An 18,000-song library scan went from 19 min 15 s to 3 min 13 s. The author: "each individual ffprobe is not that expensive". — [jellyfin PR #4242](https://github.com/jellyfin/jellyfin/pull/4242) **[2020]**
- Emby: ffprobe "sometimes … takes 15-25 seconds to get a result on a file" on busy USB disks, and ffprobe was the slowest part of a 1,593-file scan. — [Emby forum, Jun 2024](https://emby.media/community/topic/130016-scan-stuck-at-90/)
- In a Matroska file, subtitle data is interleaved through the whole file, so classifying subtitle content means reading every byte. Header-only scanning plus a title lookup did 13 files / 36 GB in 2.4 s over SMB. — [FileVerdict](https://fileverdict.com/docs/guides/scanning-speed/)
- Pure-Rust demuxer crates: `symphonia-format-mkv` and `symphonia-format-isomp4` (v0.6.1; about 9.6 M and 11.9 M downloads); `matroska-demuxer`; `mp4` (ISO/IEC 14496-12/14/17 reader/writer with `Mp4Reader::read_header`); `mp4-atom` (low-level box encoding/decoding). — [crates.io demuxer keyword](https://crates.io/keywords/demuxer), [docs.rs mp4](https://docs.rs/mp4), [docs.rs mp4-atom](https://docs.rs/mp4-atom), [matroska-demuxer](https://crates.io/crates/matroska-demuxer/reverse_dependencies)

### Inferences
- At 100 ms per ffprobe call, 100k files cost about 2.8 CPU-hours serially. Even with 8 in parallel that is about 20 minutes, mostly fixed process and seek overhead. An in-process parser that reads the first few hundred KB, plus one seek for `moov` atoms at the end of non-faststart MP4s, should cut this to the cost of 1–2 disk seeks per file. That estimate is unbenchmarked.
- Probe only new or changed files, keyed by (inode/size/mtime) and the fingerprint. Never re-probe because a sidecar file (subtitle or NFO) changed; record sidecars separately. This avoids Jellyfin's #13401 pattern.
- Keep ffmpeg/ffprobe as a fallback for exotic containers (AVI, TS, broken files) rather than a dependency on the hot path.

### Gaps
- No benchmark was found for Rust Matroska/MP4 header parsing (ms per file, bytes read) versus ffprobe.
- The symphonia crates are audio-focused. Whether they expose video track metadata (resolution, HDR, codec profile) needs checking against their docs.
- No data was found on how many real-world MP4s are non-faststart (`moov` at the end).

---

## 4. Filename parsing, metadata matching, caching and artwork

### Takeaway
Parse names offline with a Rust guessit port (`hunch`). Match against TMDB (free for non-commercial use with attribution, a soft ceiling of about 40–50 requests/s, caching capped at 6 months). Gossip the resulting IDs and metadata between nodes rather than having each node re-query. TVDB v4 needs a per-project key and either a license (free under $50k revenue) or end-user PINs at $11.99/yr. For artwork, store WebP: AVIF files are 20–30% smaller but encoding is usually an order of magnitude slower.

### Cited Findings
- **hunch** (Rust; v2.0.3 on docs.rs) is "a fast, offline Rust library … inspired by Python's guessit" and extracts 49 properties (title, year, season, episode, codecs, resolution, release group…). Earlier versions describe a span-based design of 29 matcher functions with plain function pointers. — [docs.rs hunch 2.0.3](https://docs.rs/hunch/2.0.3/hunch/index.html), [hunch 1.0.1](https://docs.rs/hunch/1.0.1/aarch64-unknown-linux-gnu/hunch)
- `torrent-parse-rank` is a Rust implementation of PTT/RTN ("The Best Damn Filename Parser"), Python-first, GPL-3.0. — [g0ldyy/torrent-parse-rank](https://github.com/g0ldyy/torrent-parse-rank). The Python `parse-torrent-title` is regex-rule based. — [platelminto/parse-torrent-title](https://github.com/platelminto/parse-torrent-title)
- **TMDB rate limits:** the legacy limit of 40 requests per 10 s was disabled on 2019-12-16. Current upper limits "sit somewhere in the 40 requests per second range" and can change; respect HTTP 429. — [TMDB rate-limiting docs (updated 2025-10-20)](https://developer.themoviedb.org/docs/rate-limiting). A TMDB moderator quotes the admin as saying a CDN enforces about 50 req/s and 20 connections per IP. — [TMDB forum](https://www.themoviedb.org/talk/67d96b2cbb433951a736693f). These two sources conflict slightly (40 vs 50).
- **TMDB terms:** you must not "Cache, for longer than 6 months, any information obtained through or from TMDB"; you must not cloak your app's identity or use excessive bandwidth; using TMDB content to train AI/ML is prohibited. — [TMDB API Terms](https://www.themoviedb.org/api-terms-of-use). The API is free for non-commercial use with attribution; commercial use requires a license. — [TMDB FAQ](https://developer.themoviedb.org/docs/faq). A 2026 forum thread asks whether storing metadata in a user's personal library falls under the 6-month rule; no answer was captured. — [TMDB talk](https://www.themoviedb.org/talk/6ac825fa562024b498444de6)
- **TVDB v4:** API keys are per project, "not for use by individual users". End users of "user-supported" projects need a subscriber PIN. — [TVDB KB](https://support.thetvdb.com/kb/faq.php?id=62). The user subscription costs $11.99/year. — [thetvdb.com/subscribe](https://www.thetvdb.com/subscribe). The licensing fee is free for companies under $50k/yr revenue (attribution required), with discounts for FOSS. — [TVDB API information](https://www.thetvdb.com/api-information)
- **Artwork formats:** AVIF is typically 20–30% smaller than WebP at equal quality, while AVIF encoding is "often 10–50× slower" at equal effort. Thumbnail-size examples: JPG 60 KB, WebP 40 KB, AVIF 28 KB. — [webpery comparison](https://www.webpery.com/compare/webp-vs-avif) (vendor blog)
- Ctrl.blog, 600 images: AVIF median –50.3% vs JPEG at equal DSSIM, WebP –31.5%. — [Ctrl blog](https://www.ctrl.blog/entry/webp-avif-comparison.html) **[2020–21]**
- WASM, 512×512 image: WebP encode 48.8 ms / 56.6 KB vs AVIF (rav1e, speed 8) 373.6 ms / 25.2 KB. — [convert.now benchmarks, May 2026](https://convert.now/benchmarks)
- **Contradiction:** on about 700 photos, libaom AVIF at speed 9 (2 min 05 s, 488 MiB) was faster than WebP `-m4` (6 min 48 s, 502 MiB). rav1e and SVT at s9 were far slower: 39 and 53 min. — [r/DataHoarder, Mar 2025](https://www.reddit.com/r/DataHoarder/comments/1ji1x4w/some_recentish_informal_tests_of_avif_jpegxl_webp/). Encoder choice and preset matter more than format.
- Plex's data directory is mostly artwork (`Metadata` and `Media`), and "several gigabytes" is common. — [Plex support](https://support.plex.tv/articles/202529153-why-is-my-plex-media-server-directory-so-large/). One Plex install reached 500+ GB from chapter index thumbnails, and another had 35 GB across 1.6 million files (Emby: 6.0 GB, 4,504 items, for the same library). — [Plex forum 2021](https://forums.plex.tv/t/why-is-my-plex-media-server-directory-so-large/725768) **[old]**

### Inferences
- In a distributed design, metadata should be fetched once by the node that first sees a title, then replicated to peers by TMDB ID. That avoids N× API load. A peer-to-peer cache that refreshes within 6 months stays within TMDB's caching term as long as entries are re-fetched or expired. Whether redistributing TMDB data across a user's own nodes counts as "caching" or as redistribution is a legal question.
- Artwork dominates disk use (Plex). Store one poster size (e.g., about 342 px wide) and one backdrop size as WebP. Generate others on demand, or let clients fetch TMDB's CDN sizes directly. Skip chapter and trickplay thumbnails by default, since that is where Plex's hundreds of GB come from.
- Fast-preset libaom AVIF can be competitive with WebP in encode time. If AVIF is wanted, benchmark the encoder you actually ship (rav1e is pure Rust but slow; libaom/SVT need C dependencies).

### Gaps
- No accuracy or speed benchmark of hunch vs guessit vs PTT was found.
- TMDB's image CDN terms and bandwidth limits were not checked.
- No sourced number was found for Jellyfin or Plex metadata directory size per 1,000 items in 2024–26.

---

## 5. Embedded storage engine and search

### Takeaway
Use SQLite (WAL, `synchronous=NORMAL`, `mmap_size`, a single writer thread, batched transactions) for the catalog and user state. You get ad-hoc queries, FTS5 and battle-tested crash safety, and Jellyfin's and Navidrome's locking problems come from many concurrent writers and long write transactions, not from SQLite itself. redb (pure Rust, copy-on-write B-tree, ACID) and fjall 3 (LSM, 2.2 MB binary) suit replication logs and KV state. LMDB/heed has the fastest reads. sled is not crash-safe enough. For 100k titles, in-memory fuzzy matching (nucleo) takes under 10 ms per keystroke, and tantivy is only needed for large full-text search.

### Cited Findings
**Key-value engines (redb README benchmark, Ryzen 9950X3D, Samsung 9100 PRO NVMe; lower is better)** — [redb README](https://raw.githubusercontent.com/cberner/redb/master/README.md)

| | redb | lmdb | rocksdb | fjall | sqlite |
|---|---|---|---|---|---|
| bulk load | 17,063 ms | **9,232** | 13,969 | 18,619 | 15,341 |
| individual writes | **920 ms** | 1,598 | 2,432 | 3,488 | 7,040 |
| batch writes | 1,595 ms | 942 | 451 | **353** | 2,625 |
| random reads | 1,138 ms | **637** | 2,911 | 2,177 | 4,283 |
| random reads, 32 threads | 410 ms | **125** | 1,100 | 576 | 26,536 |
| compacted size | 1.69 GiB | 1.26 GiB | **454.71 MiB** | 1000.95 MiB | 556.85 MiB |

- redb features: zero-copy, thread-safe BTreeMap API; fully ACID; MVCC with non-blocking readers and a single writer; "crash-safe by default"; savepoints; stable file format; multi-process access. — [redb README](https://raw.githubusercontent.com/cberner/redb/master/README.md)
- **fjall 3.0** (released 15 Sep 2026): LSM-tree, written in Rust, about 42k LoC. Embedding it builds a "~2.2 MB binary" in about 3.5 s, vs RocksDB at about 12 MB and about 40 s. It verifies 128-bit xxh3 checksums on every block load, uses an exclusive file lock (single process only), and unpins filters and indexes so memory does not grow linearly with key count. Its author calls sled "alpha stage" and excludes it from fsync benchmarks because it "does not reliably fsync". In the feed workload, "SQLite is really not happy … basically stopping to work completely, but allocating tons of disk space." — [fjall 3 release post](https://fjall-rs.github.io/post/fjall-3)
- In that post, LMDB memory is shown as mmap rather than RSS, and LMDB and SQLite do not checksum pages on read. — [fjall 3 post](https://fjall-rs.github.io/post/fjall-3)

**SQLite tuning**
- 5,000 single-insert transactions: DELETE+FULL 746/s, WAL+FULL 3,662/s, WAL+NORMAL 83,401/s. 200k random primary-key lookups on a 224 MiB DB: default 245k/s, `cache_size=64MiB` 218k/s (no gain), `mmap_size=512MiB` 312k/s (+27%). — [ZeroExp](https://zeroexp.dev/library/sqlite-pragmas-web-app)
- A read-heavy site on SQLite (WAL, mmap 256 MB, 200k rows) did about 170k slug lookups/s on one process and about 861k/s on 8 processes. — [ZeroExp read-heavy](https://zeroexp.dev/library/sqlite-read-heavy-site)
- SQLite's documentation: WAL with `synchronous=NORMAL` is corruption-safe. A power loss may roll back recent commits, but "the database will not be corrupted". — [sqlite.org compile options](https://www.sqlite.org/compile.html)
- The phiresky recipe is `journal_mode=WAL; synchronous=normal; temp_store=memory; mmap_size=30000000000`. With many writes, the WAL can grow without bound unless you run `wal_checkpoint(truncate)` periodically. — [phiresky blog](https://phiresky.github.io/blog/2020/sqlite-performance-tuning/) **[2020]**
- **Jellyfin's lesson:** "In versions prior to 10.11 Jellyfin had a bug in its parallel task limit which resulted in exponential overscheduling of library scan operations which hammered the database engine with thousands of parallel write requests that an SQLite engine is simply not able" to handle. Jellyfin 10.11 offers selectable locking modes (No-Lock, Optimistic, Pessimistic). — [Jellyfin blog "SQLite locking", Oct 2025](https://jellyfin.org/posts/SQLite-locking). Jellyfin now exposes `cacheSize`, `lockingmode` and arbitrary `#PRAGMA:` options. — [Jellyfin configuration docs](https://jellyfin.org/docs/general/administration/configuration.html/)
- Jellyfin 10.11 finished migrating its library DB to EF Core. The release notes recommend keeping the Library Page Size at the default 100 "for performance reasons". — [Jellyfin 10.11.0 release](https://jellyfin.org/posts/jellyfin-release-10.11.0/)

**Search**
- tantivy vs FTS5 (Project Gutenberg, about 155k docs, about 1 GB corpus, 4 threads): tantivy 85,543 q/s at p50 9 µs vs FTS5 3,191 q/s at p50 299 µs. Index size: 1.4 GB vs 1.63 GB. — [sqlite-tantivy BENCHMARK_RESULTS](https://github.com/russellromney/sqlite-tantivy/blob/main/BENCHMARK_RESULTS.md) (third-party)
- tantivy's stated design goals: "Search should be O(1) in memory. Indexing should be O(1) in memory"; BM25 scoring; designed for `MmapDirectory`. — [tantivy ARCHITECTURE.md](https://github.com/quickwit-oss/tantivy/blob/main/ARCHITECTURE.md)
- nucleo-matcher on 95,655 haystacks (median 37 chars), needle "emacs", single-threaded: 7.43 ms vs telescope-fzf-native 9.10 ms. nucleo micro-benchmarks over Linux kernel paths: 2.1–9.5 ms vs skim 16.9–35.5 ms. — [noib3/fuzzy-benches](https://github.com/noib3/fuzzy-benches/), [nucleo README](https://github.com/helix-editor/nucleo)
- On 1.4 M Chromium paths, frizbee sequential took 22.36 ms vs nucleo 90.53 ms. The fzf numbers in that harness are disputed. — [frizbee BENCHMARKS](https://github.com/saghen/frizbee/blob/main/BENCHMARKS.md), [Sparkles survey](https://sparkles-docs.pages.dev/research/fuzzy-matching/comparison)

### Inferences
- Catalog size: 100k files × a few KB of rows (streams, IDs, paths) is roughly 100–500 MB of SQLite, which fits in the page cache, so browse queries are sub-millisecond. The Jellyfin "10–100 GB" figure comes from its schema, images and blobs, not from the inherent data volume.
- The recommended pattern is one dedicated writer task that batches scan results into transactions of about 1,000 rows, plus a pool of read-only connections. This avoids the `database is locked` failures seen in Jellyfin 10.11 and Navidrome 0.64.
- For cross-node replication of user state (watched flags, resume positions), a CRDT/op-log in redb or fjall, or SQLite tables with HLC timestamps, would work. DuckDB is OLAP-oriented and adds a large dependency; it is not justified here (no data found, see Gaps).
- Search: keep a compact in-memory list of titles, people and aliases (100k × ~64 B ≈ 6 MB) and use nucleo-matcher for type-ahead (single-digit ms). Use SQLite FTS5 for overview and plot text. tantivy's speed advantage matters only at larger scale and costs binary size and complexity.

### Gaps
- No 2024–26 DuckDB numbers for this workload were found, and no idle-memory numbers for redb, fjall or SQLite at catalog scale.
- No measured tantivy binary-size contribution or idle RSS.
- The redb benchmark is from the redb author on NVMe. No HDD-backed results were found.

---

## 6. Real-world footprints and what makes Navidrome-style servers light

### Takeaway
On a fresh install, Plex and Jellyfin idle at about 200–250 MB, but with real libraries they grow to hundreds of MB or several GB (Jellyfin's .NET caching, Emby balloon reports), and their data directories grow to tens or hundreds of GB. Navidrome (a Go binary) claims under 50 MB RAM for 29k songs. It stays light by importing a fixed set of tags, skipping unchanged folders by hash, storing everything in one SQLite file, and keeping media analysis out of the hot path.

### Cited Findings
- Fresh install, empty library, measured Oct 2026: Jellyfin 12.3.0 idle 193 MB RAM with a 1,639 MB image; Plex 1.43.5 idle 237 MB RAM with 346 MB on disk. — [SizeMyServer](https://sizemyserver.io/compare/jellyfin-vs-plex/), [Plex page](https://sizemyserver.io/apps/plex/)
- Plex's docs (as summarized by SizeMyServer): the config database is "usually a few GB but can reach hundreds of GB for large libraries"; 4 GB RAM is typically more than sufficient. — [SizeMyServer Plex](https://sizemyserver.io/apps/plex/) (secondary source)
- Jellyfin docs: "A database for a moderate-sized library can grow anywhere from 10 to 100 GB." — [Jellyfin storage docs](https://jellyfin.org/docs/general/administration/storage). Whether this counts metadata and images is unclear from the excerpt.
- Jellyfin memory: an unmerged locking PR in the 10.11 cycle cut RAM use "from 6 GB or so to < 1GB". — [jellyfin#14095](https://github.com/jellyfin/jellyfin/issues/14095) (May 2025). `MALLOC_TRIM_THRESHOLD_=131072` in the official image cut one user's RSS from about 10 GB after weeks of uptime to about 700 MB. — [Jellyfin forum, Feb 2024](https://forum.jellyfin.org/t-excessive-ram-consumption)
- Emby: a user's server sat at about 3,000 MB idle, vs 128 MB after rolling back. RAM balloons have been tied to strm loops, overlapping libraries and failing screen-grabber ffprobe runs. — [Emby forum 2023](https://emby.media/community/topic/123238-windows-server-2019-with-excessive-memory-usage), [Emby forum 2023](https://emby.media/community/topic/118367-resolved-ram-utilization-balloons-in-emby-server-v-47110)
- In 2019, the same library took 1.7 GB under Emby vs 6.3 GB (and growing) under Jellyfin. — [jellyfin#1655](https://github.com/jellyfin/jellyfin/issues/1655) **[old]**
- Plex: a DB of 7 GB (both files) runs fine on a Xeon. Another user with 4,000 movies, 20,000 episodes and 40k+ tracks reports slowdowns and blames SQLite. — [Plex forum, May 2024](https://forums.plex.tv/t/increase-use-of-available-resources/876694). Plex's default "Database cache size" is 40 MB; one user had a 12.4 GB metadata folder. — [Plex forum 2023](https://forums.plex.tv/t/database-cache-size-mb-optimal-setting-for-large-library/852506)
- Plex library-size example: 90,403 media items and 224,811 media streams. — [r/PleX](https://www.reddit.com/r/PleX/comments/evn4rj/how_large_is_your_plex_library) **[~2020]**
- Navidrome claims "Very low resource usage: Ex: with a library of 300GB (~29000 songs), it uses less than 50MB of RAM". — [Docker Hub deluan/navidrome](https://hub.docker.com/r/deluan/navidrome). A 2026 guide says under 100 MB idle and a single SQLite file. — [gilricardo.com, Aug 2026](https://www.gilricardo.com/blog/navidrome-proxmox-lxc-2026)
- Counter-evidence for Navidrome: a full scan hit 10 GB RAM for one user while another peaked at 180 MB (pre-new-scanner, Oct 2024). — [navidrome#3408](https://github.com/navidrome/navidrome/issues/3408). An idle instance grew to 2.6 GB (2022, default 1-minute scan schedule). — [navidrome#2003](https://github.com/navidrome/navidrome/issues/2003). A 2026 NAS user says Navidrome is their top idle memory user. — [navidrome discussion #5289](https://github.com/navidrome/navidrome/discussions/5289)
- Navidrome imports only "a predefined set of tags" "to improve performance and reduce storage requirements". — [Navidrome usage docs](https://www.navidrome.org/_print/docs/usage)
- Kodi shares a library across clients only through MySQL/MariaDB. The feature is "advanced (complicated) and experimental", and every client must run the same Kodi version. — [Kodi wiki MySQL](https://kodi.wiki/view/MySQL)

### Inferences
- What makes a server light, based on Navidrome and the failures above:
  1. A bounded, fixed schema; no per-item JSON blobs.
  2. Folder-level change hashes, so unchanged subtrees are never opened.
  3. No eager ffmpeg work: chapter thumbnails, trickplay, loudness and intro detection off by default or lazy.
  4. A single writer with bounded concurrency per disk.
  5. A native binary with no managed-runtime heap; Jellyfin's .NET heap and glibc arena behaviour cause much of its RSS.
  6. Artwork stored at one or two small sizes.
- For the target binary, a realistic budget is about 20–60 MB RSS idle with a 100k-file catalog: SQLite page cache plus an in-memory title index of about 6 MB plus the Tokio runtime. That is an estimate and has not been measured.
- Kodi's single shared MySQL is the anti-pattern for a no-central-server design. Each node should own its own catalog and replicate summaries and user state.

### Gaps
- No independent 2025–26 measurement of Plex, Jellyfin, Emby or Navidrome RAM, CPU and DB size on the same 10k–100k-item library.
- Idle CPU and wakeups (periodic timers, scheduled tasks) were not quantified for any server.
- The SizeMyServer numbers are empty-library measurements only.
