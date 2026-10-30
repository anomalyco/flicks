# Minimizing time-to-first-frame and seek latency for personal video (late 2026)

Scope: delivery formats, remux strategies, how existing servers/clients start and seek, Rust remux options, scan-time indexing, pre-buffering, and API shape. The goal is the fastest playback start and seek with the least server CPU. Dates are given where known; pre-2024 data is marked **[older]**.

**Local measurement used throughout (call it "BENCH")**: run by this researcher on 2026-10-30 on an Apple M5 Max with NVMe, FFmpeg 9.0.2 (Homebrew), and a **warm OS page cache**. Test file: synthetic 20-minute 1080p24 H.264 (8 Mb/s, 2 s GOP, i.e. 600 keyframes) + AAC stereo in MKV, written by FFmpeg's Matroska muxer, 1.23 GB. The method was Python `subprocess` timing, median of 3–10 runs. The "first segment ready" time is when the HLS playlist first lists a finished segment. These numbers are a best case (fast CPU, cached file). On a cold HDD each random seek adds about 5–15 ms of disk latency, and throughput drops to about 150–250 MB/s. Those disk figures are general knowledge and were not measured here.

| BENCH measurement | Result |
|---|---|
| Spawn `ffmpeg -version` (process start only) | **27 ms** |
| `ffprobe -show_streams -show_format` on the MKV | **43 ms** |
| ffmpeg `-c copy` → HLS fMP4, `hls_time 6`, from 0 s: first segment listed | **48 ms** |
| Same, with input seek `-ss 600` (mid-file): first segment listed | **44 ms** |
| Same in MPEG-TS segments (0 s / 600 s) | **47 / 43 ms** |
| Full remux of the whole 20-min / 1.23 GB file to fMP4 HLS | **0.94 s** (≈1.3 GB/s, I/O-bound) |
| Keyframe index via `ffprobe -show_entries packet=pts_time,pos,flags` (28,800 video packets) | **394 ms** |
| Size of MKV `Cues` element (600 keyframes) written by FFmpeg | **13 KB**, located at the file tail via the SeekHead in the first 4 KB; raw read **0.28 ms** |

---

## 1. Delivery options (progressive byte-range vs on-the-fly fMP4/CMAF, HLS TS vs fMP4, DASH, LL-HLS): which starts and seeks fastest per client, and what segment duration?

### Takeaway
On LAN or home-server links, the fastest path is direct play of the original file over HTTP byte ranges. It costs no server CPU. Startup takes a few round trips, and a seek is a single range request once the client has the index. When the client can't demux the container (mainly Safari/AVPlayer and browsers with MKV), the next-fastest is **stateless, keyframe-aligned fMP4 (CMAF) HLS remuxed in-process**. Use segments of about 4–6 s; the first segments may be shorter. TS segments add 5–15% container overhead and can't carry HEVC in HLS. LL-HLS and chunked CMAF target live latency and don't help VOD start or seek.

### Cited Findings
- Apple HLS authoring spec: "Target durations SHOULD be 6 seconds"; segments "SHOULD be nominally 6 seconds"; "Video segments MUST start with an IDR frame"; segments "MUST NOT exceed the target duration by more than 0.5 seconds"; fMP4 decode times must be continuous across segments. — [Apple HLS authoring spec](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices)
- Apple's guidance as summarized by a CDN: 6 s VOD segments with a 2 s keyframe interval (48 frames at 24 fps). A 2-hour film at 6 s is 1,200 segments per rendition. Shorter segments start faster but each must begin on a keyframe. — [BlazingCDN HLS encoding guide (2026)](https://blog.blazingcdn.com/en-us/hls-encoding-segment-length-keyframes-and-renditions)
- "Most players buffer three segments before playback begins, so time-to-first-frame is roughly 3 × segment_duration plus network"; the segment length is "the worst-case seek granularity"; 6 s is the safe VOD default, 4 s "when finer seeking matters". This is a secondary blog; the 3-segment rule mostly describes live HLS players. — [Media Pipeline, Tuning HLS Segment Duration for Seek Latency (2025-06)](https://www.mediapipeline.org/media-delivery-publishing-automation/adaptive-bitrate-packaging/tuning-hls-segment-duration-for-seek-latency)
- ExoPlayer/Media3 `DefaultLoadControl`: `DEFAULT_BUFFER_FOR_PLAYBACK_MS = 2500` (media that must be buffered before playback starts or resumes after a seek), `DEFAULT_BUFFER_FOR_PLAYBACK_AFTER_REBUFFER_MS = 5000`, `DEFAULT_MAX_BUFFER_MS = 50_000`. The 2026 Media3 API adds separate buffer parameters "for local on-device playback". — [ExoPlayer DefaultLoadControl source](https://github.com/google/ExoPlayer/blob/release-v2/library/core/src/main/java/com/google/android/exoplayer2/DefaultLoadControl.java); [Media3 DefaultLoadControl (2026-08)](https://developer.android.com/reference/kotlin/androidx/media3/exoplayer/DefaultLoadControl)
- MPEG-TS for segmented delivery has "overhead ratios between 5% and 15%, far higher than fMP4". CMAF itself doesn't cut latency; only its chunked low-latency mode does. — [Dolby OptiView / THEO, Low Latency with Chunked CMAF (2018, updated 2025)](https://optiview.dolby.com/resources/blog/streaming/low-latency-chunked-cmaf) **[older origin]**; similar "up to 15% more overhead" — [Vidbeo (2017)](https://www.vidbeo.com/blog/dash-hls-and-hls-fmp4) **[older]**
- fMP4 "is required for HEVC and modern DRM in HLS". One fMP4 segment set serves both HLS and DASH, with an init segment referenced by `#EXT-X-MAP`. — [5centsCDN, MPEG-TS vs fMP4 (2026-07)](https://www.5centscdn.net/blog/mpeg-ts-vs-fmp4)
- An academic live study found that replacing .ts with .fMP4 and shortening segments (6→4→2 s) "reduces live latency significantly" across Web, iOS, Android, Smart TV and Apple TV clients. — [ACM MMSys paper on CMAF](https://dl.acm.org/doi/pdf/10.1145/3524273.3532904)
- LL-HLS was designed for "one to two seconds delay from live", using partial segments ("Parts"). Its goal is live latency, not VOD start. — [Apple WWDC19 Introducing Low-Latency HLS](https://developer.apple.com/la/videos/play/wwdc2019/502) **[older]**
- Bitmovin: "The major contributor to VST [video start time] is network latency." The player must complete several requests (manifest(s), init, first segments) before the first frame, and the startup rendition's bitrate adds download time. — [Bitmovin Community, Best practices for optimizing VST (2023)](https://community.bitmovin.com/t/best-practices-for-optimizing-video-start-time-vst/1655) **[older]**
- Mux defines Video Startup Time as the time "from when the viewer intended the video to play, to when the first frame of the video is displayed". Common slow-start causes include DNS, TCP, TLS and a cold first segment or manifest. — [Mux, Video Startup Time explained](https://www.mux.com/blog/the-video-startup-time-metric-explained) **[older, 2017]**; [Mux analytics guide](https://www.mux.com/articles/video-analytics-for-developers-guide)
- Plex staff on direct play: "After the initial startup, a direct play is a single TCP connection allowing the window to grow to large enough sizes the latency becomes a non-factor in the transfer speed." — [Plex forum, gbooker02 (2025-06)](https://forums.plex.tv/t/buffer-too-small-in-remote-clients/923931)
- nginx-vod-module (Kaltura) remuxes MP4 into HLS/DASH segments per request, with its own packager instead of FFmpeg ("for performance reasons"). It supports variable segment lengths "enabling the player to select the optimal bitrate fast, without the overhead of short segments for the whole duration". It recommends a large `vod_metadata_cache` so each segment request doesn't re-read the moov. — [nginx-vod-module README](https://github.com/kaltura/nginx-vod-module/); [issue #1508](https://github.com/kaltura/nginx-vod-module/issues/1508)
- BENCH: remux CPU is negligible. A full 1.23 GB, 20-min file remuxed to fMP4 HLS in 0.94 s, and the first 6 s segment was ready 44–48 ms after spawn, even with a mid-file `-ss 600` seek. TS and fMP4 timings were the same (±5 ms).

### Inferences
- **Per client type (inference from the above):**
  - (a) Clients that demux MKV/MP4 themselves (Infuse, Kodi, VLC/libVLC, mpv, ExoPlayer/Media3): use direct byte-range play. Startup is about 2–4 RTTs: header read, then on non-faststart files a jump to the tail index (Cues or moov), then the first cluster. A seek is 1 RTT plus decode from the nearest keyframe.
  - (b) Apple AVPlayer (Safari, tvOS and iOS native player) and Chromium browsers fed MKV: they need MP4/fMP4 or HLS. Serve on-the-fly fMP4 HLS, or a faststart MP4 by range when the source is already MP4/MOV.
  - (c) The browser MSE path (hls.js, Shaka) can use the same fMP4 HLS or DASH segments.
- DASH vs HLS: with CMAF the media bytes are identical, so the choice only affects the manifest. AVPlayer requires HLS. Serve HLS fMP4 everywhere and add a DASH MPD only if a specific client needs it.
- Segment length: for remuxed VOD, segment boundaries have to land on source keyframes, so "6 s" really means "the first keyframe at or after 6 s". With stateless per-segment remux, segment length barely affects server cost, so 4 s is reasonable. Making the first 1–2 segments about 2 s (variable lengths, as nginx-vod does) lets players reach their start threshold (for example ExoPlayer's 2.5 s) with one small download.
- Startup is dominated by round trips and server-side session setup, not container conversion. On LAN the RTT is under 1 ms, so the floor is TTFB + first-segment transfer + decoder init, likely about 100–300 ms. This was not measured end to end here.

**Recommended fastest default path (synthesis):**
1. Direct play whenever the client can demux the file: zero CPU, range-based.
2. Otherwise use in-process, stateless, keyframe-aligned CMAF/fMP4 HLS, with segment N computed from a precomputed index and no per-session process.
3. ffmpeg transcode only when a codec is unsupported.

### Gaps
- No controlled, published 2024–2026 TTFF measurements compare progressive vs HLS vs DASH on the same LAN content and clients. The numbers above are component timings, not end-to-end player TTFF.
- I didn't verify hls.js, Shaka or AVPlayer startup buffer thresholds from primary docs. AVPlayer's `preferredForwardBufferDuration` default isn't documented numerically in the page found.
- The nginx-vod "1,950 req/s, 202 ms avg latency at 400 connections through CloudFront" figure ([issue #1400](https://github.com/kaltura/nginx-vod-module/issues/1400)) has unclear context (CDN, DRM), so it isn't usable as a per-segment latency benchmark.

---

## 2. How do Plex, Jellyfin, Emby, Infuse and Kodi start playback and seek? Known latencies, direct-stream overheads, transcoder spin-up, slow-start complaints

### Takeaway
All server-centric systems (Plex, Jellyfin, Emby) negotiate first: a PlaybackInfo/decision call, then a playlist, then segments. For any remux or transcode they spawn an ffmpeg (or Plex Transcoder) process per session, and they restart it on seeks outside the produced range. The documented slow starts (5 s to minutes) come mostly from process lifecycle, playlist/segment mismatches, throttling and network/TLS/DNS issues, not remux CPU. Infuse and Kodi just open the original file and range-read it. Infuse played direct-only until v8.5 (July 2026).

### Cited Findings
**Jellyfin**
- Playback types, ordered by server load: Direct Play ("almost no additional load"), Remux (container change), Direct Stream (audio transcoded), Transcode. Clients send device profiles and the server decides. — [Jellyfin docs, Transcoding](https://jellyfin.org/docs/general/post-install/transcoding/)
- Example Jellyfin 10.10 remux command: `-codec:v:0 copy ... -f hls -hls_time 6 -hls_segment_type fmp4`. The user reports start/scrub times of "12s -> 3 minutes", "consistently slower when transcoding, but still often slow when direct streaming". The log line reads: "Current HLS implementation doesn't support non-keyframe breaks". — [Jellyfin forum, Tracking/Starting Slow (2025-01)](https://forum.jellyfin.org/t-solved-tracking-starting-slow)
- Seeking past the transcoded region: "ffmpeg will be restarted at that time stamp and there is usually a noticeable delay of a few seconds". — [Jellyfin forum (2024-10)](https://forum.jellyfin.org/t-how-does-jellyfin-live-transcoding)
- On 10.8.13, a resume ran ffmpeg twice in sequence: segment 0 first, then a cancel and restart at the resume point. Fixed in 2024 unstable. — [jellyfin#11025 (2024-02)](https://github.com/jellyfin/jellyfin/issues/11025)
- Remux ignores the transcode throttle: "Jellyfin/ffmpeg remuxes the whole file at once without throttling, causing lag before the playback starts" (NFS storage on a 1 Gb link). — [jellyfin#6299 (2021)](https://github.com/jellyfin/jellyfin/issues/6299) **[older]**
- HLS remux seek bug with Dolby Vision on iOS: after a seek, "FFmpeg restarts with correct `-ss` position and `-start_number 1187`" but "the m3u8 still shows" the old segments, so the client buffers forever. — [Jellyfin issue search result (keyframe/remux)](https://api.github.com/search/issues?q=repo%3Ajellyfin%2Fjellyfin+is%3Aissue+keyframe+extraction&sort=created&order=desc&per_page=20)
- Jellyfin 10.11 (2026) still has edge cases. With an infrequent-keyframe source, input-side `-ss` with stream copy near the end of the file produces identical MPEG-TS for different seek points, so the ending repeats. — [jellyfin#18154 (2026-09)](https://github.com/jellyfin/jellyfin/issues/18154)
- Playlist and segment mismatch: when remuxing, "Jellyfin generates the playlist based on key-frames, so the segment length used for generating the manifest must match what FFmpeg is using… Some native HLS clients may stall". — [jellyfin PR #16341 (2026)](https://github.com/jellyfin/jellyfin/pull/16341)
- `-noaccurate_seek` refactor: for remux, "segment starts are aligned with key frames in the source media, so non-accurate seeking has the same result as accurate seeking". — [jellyfin PR #15926](https://github.com/jellyfin/jellyfin/pull/15926)
- Direct-play complaint in 10.11 (Chrome on M1, 70 Mb/s remux file): buffering stalls. A commenter observed that direct play "takes much larger chunks", "only buffers one chunk ahead" and fetches "at the last possible moment". — [jellyfin#15237 (2025-10)](https://github.com/jellyfin/jellyfin/issues/15237)
- Swiftfin (VLCKit player) took 60–90 s to start behind nginx vs about 5 s without the proxy. Maintainers pointed at TLS 1.3 support in VLCKit. A 2026 report: about 20 s start on tvOS via `server.local:8096` vs under 5 s when using the IP address, i.e. name resolution. The issue was closed by the move to SwiftVLC (PR #2273/#2382, Oct 2026). Seeking "works instantly" once playing. — [Swiftfin#1444](https://github.com/jellyfin/Swiftfin/issues/1444)
- Live TV remux/transcode through Jellyfin took "exactly 1 minute and 45 seconds" to start (`-hls_time 3 -hls_segment_type mpegts`). — [Jellyfin forum (2024-10)](https://forum.jellyfin.org/t-live-tv-slow-to-start)

**Plex**
- Three modes: Direct Play (file as-is), Direct Stream (container/stream adjustments), Transcode. — [Plex support, Direct Play/Direct Stream/Transcoding](https://support.plex.tv/articles/200430303-streaming-overview/)
- "Transcoder default throttle buffer" defaults to 60 s: "Amount in seconds to buffer before throttling back the transcoder speed". Plex staff: the transcoder "quickly transcodes a block of time then sleeps". — [Plex support, Transcoder](https://support.plex.tv/articles/transcoder); [Plex forum](https://forums.plex.tv/t/transcoder-default-throttle-buffer-in-pms-settings/207735) **[older]**
- `SegmentedTranscoderTimeout` defaults to 20 s: "Timeout in seconds segmented transcodes wait for the transcoder to begin writing data". — [WebTools-NG wiki, PMS transcoder settings](https://github.com/WebTools-NG/WebTools-NG/wiki/transcoder)
- Plex universal-transcode flow: `GET /video/:/transcode/universal/decision`, then `start.m3u8?...&fastSeek=1&directStream=1&offset=…`, then `session/<id>/base/index.m3u8`, then segments. A third-party Rust client's parser comments that the index playlist uses `#EXTINF:1, nodesc`, i.e. **1-second segments**. This is unverified by Plex docs. — [plexclip crate source](https://docs.rs/crate/plexclip/latest/source/src/plex/transcode.rs); [plex-api crate](https://docs.rs/plex-api/latest/src/plex_api/server/transcode/session.rs.html)
- User reports: playback that needs a transcode has an "initial load of anywhere between 10 and 45 seconds", then never buffers. — [AVS Forum (2024-01)](https://www.avsforum.com/threads/plex-slow-to-start-playback.3293824). An N100 hardware-transcodes to 720p at about 5–7× real time "and throttle[s] within a few seconds"; staff found the logs showed "multi-second times before segments are given to the client". — [Plex forum (2025-06)](https://forums.plex.tv/t/buffer-too-small-in-remote-clients/923931)

**Infuse**
- Infuse plays "MKV, MP4, ISO, VIDEO_TS, BDMV — without transcoding", over SMB/NFS/WebDAV or via Plex/Emby/Jellyfin. — [Firecore Infuse page](https://firecore.com/infuse)
- Until v8.5, Infuse always direct-played from media servers. A Jellyfin dev read the logs as Infuse "directly accessing the media, perhaps via the 'download' endpoint… no checks for 'locality', no application of bit rate controls… and transcoding". — [Firecore community (2024-12 to 2025-06)](https://community.firecore.com/t/support-jellyfin-remote-access-limits/53047)
- Infuse 8.5 (2026-07-30) added transcoding options for Plex/Emby/Jellyfin. The default still "play[s] the original video at full quality when pressing Play"; transcoding sits behind a long-press. — [Firecore blog, Infuse 8.5](https://firecore.com/blog/infuse-85-smarter-streaming); [release notes](https://firecore.com/releases)
- Infuse 7.7 "Direct Mode" (2024-02) fetches library data on demand instead of pre-caching, for "near-instant access". This is a browse-latency feature, not a playback one. — [Firecore blog, Infuse 7.7](https://firecore.com/blog/infuse-77-direct-mode)

**Kodi**
- Kodi's cache has a buffer mode (none, internet only, all network filesystems including SMB/NFS, or all), a memory size, and a read factor. These used to live in advancedsettings.xml. — [Kodi wiki, Caching](https://kodi.wiki/view/Settings/Services/Caching)
- "The default cache Kodi uses is 20mb, which requires 60mb of free RAM" (forum, not official). — [CoreELEC forum (2020)](https://discourse.coreelec.org/t/be-careful-of-altering-the-caching-settings/11408) **[older]**
- A Kodi/Vero 4K user saw a direct-play 4K HDR start "within a couple seconds" after fixing Plex add-on settings, vs "instantly" before an upgrade. — [OSMC forum (2021)](https://discourse.osmc.tv/t/vero-4k-kodi-19-plex-slow-plex-loading-movies/90030) **[older]**

### Inferences
- BENCH shows ffmpeg spawn + probe + first remuxed segment takes about 45–90 ms on fast hardware. So the multi-second to multi-minute delays reported for Jellyfin and Plex can't be explained by remux CPU. They come from session orchestration: double starts, restart-on-seek, playlist/segment mismatches, waiting on throttled or stalled jobs, and timeouts (20 s), plus network/TLS/mDNS problems in clients.
- The best-performing clients by user reputation (Infuse, Kodi) avoid server orchestration entirely and range-read the file. That is the model to copy for capable clients.
- Emby: I found no specific 2024–2026 startup data. Its architecture is close to Jellyfin's (Jellyfin forked from Emby), so the same session-based remux model is likely.

### Gaps
- No reliable, controlled TTFF numbers (in ms) for Plex, Jellyfin, Emby or Infuse direct play on LAN. The evidence is anecdotal user reports.
- I couldn't confirm Plex's actual HLS/DASH segment duration from Plex documentation. The 1 s figure comes from a third-party parser.
- No Emby-specific sources.

---

## 3. Remuxing in Rust without spawning ffmpeg: crates, performance, maturity; spawn vs in-process cost

### Takeaway
No single mature pure-Rust "MKV → fMP4 remuxer" exists. The pure-Rust pieces are a demuxer (matroska-demuxer, symphonia-format-mkv/isomp4, re_mp4/mp4) plus a muxer (muxide, mp4), and you'd write the glue: codec-private conversion, timestamp handling, B-frame composition offsets. The mature in-process route is the FFmpeg libraries via `ffmpeg-next` (maintenance mode, but tracks FFmpeg 9) or `rsmpeg` (stopped at FFmpeg 8.0). Spawning ffmpeg costs only about 27 ms per process (BENCH). What process-per-session actually costs is architectural: lifecycle, restart-on-seek and state. Per-request CPU isn't the issue.

### Cited Findings
- `ffmpeg-next` 9.0.0 (2026-08-05): "currently in maintenance mode… compatible with all of FFmpeg's versions from 3.4"; new APIs may need user PRs; "Any PR to improve existing API is unlikely to be merged". About 8.4 M total downloads. — [crates.io ffmpeg-next](https://crates.io/crates/ffmpeg-next); [docs.rs](https://docs.rs/crate/ffmpeg-next/latest)
- `rsmpeg` 0.18.0+ffmpeg.8.0 (2025-08-24): a "thin and safe layer" exposing FFmpeg internals, around 109k downloads. A downstream project (2026-08) switched from rsmpeg to ffmpeg-next because "rsmpeg 0.18 caps at FFmpeg 8.0 and fails to compile against FFmpeg 9". — [crates.io rsmpeg](https://crates.io/crates/rsmpeg/0.18.0+ffmpeg.8.0); [avmux commit](https://github.com/kingwingfly/avmux/commit/f544e5e4bedfba91382534075cb19975db1c6a18)
- `symphonia`: pure Rust, "100% safe Rust". Demuxer status: ISO/MP4 "Great", MKV/WebM "Good". Its decoders are audio-only and "generally +/-15% the performance of FFMpeg". 0.6.0-alpha.2 exists with MSRV 1.85. — [Symphonia GitHub](https://github.com/pdeljanov/Symphonia); [crates.io 0.6.0-alpha.2](https://crates.io/crates/symphonia/0.6.0-alpha.2)
- `matroska-demuxer` v0.8.1 (pure Rust, MIT/Apache/Zlib): "For simplicity only the elements supported by both Matroska and WebM are supported." That's a warning sign for real-world MKV features (e.g., some codec IDs and attachments). Tested against the Matroska test suite. — [crates.io matroska-demuxer](https://crates.io/crates/matroska-demuxer); [GitHub](https://github.com/hasenbanck/matroska-demuxer)
- `re_mp4` (Rerun, created 2024-09): an MP4 parser forked from `mp4`, with code from mp4box.js. Read-only and aimed at WebCodecs players. — [rerun-io/re_mp4](https://github.com/rerun-io/re_mp4)
- `mp4` (alfg/mp4-rust): an MP4 reader and writer for ISO/IEC 14496 parts. — [crates.io mp4](https://crates.io/crates/mp4)
- `muxide` 0.2.0: "Minimal-dependency pure-Rust MP4 muxer". It takes H.264/H.265/AV1/VP9 + AAC/Opus with explicit PTS/DTS, writes faststart MP4 and "Supports fragmented MP4 (fMP4) for DASH/HLS". It does not demux or handle MKV, and it "rejects invalid input" timestamps. — [docs.rs muxide 0.2.0](https://docs.rs/muxide/0.2.0/aarch64-unknown-linux-gnu/muxide)
- Mediabunny (TypeScript, zero-dependency) shows that a from-scratch demux/mux library can be fast. Its site shows "862 ops/s" vs "233 ops/s" for @remotion/media-parser on an unspecified benchmark. Its server package, however, "is a thin wrapper around NodeAV, which itself is a thin wrapper around the FFmpeg C API". — [mediabunny.dev](https://mediabunny.dev/); [@mediabunny/server](https://mediabunny.dev/guide/extensions/server)
- nginx-vod-module is a production precedent for a custom, non-FFmpeg packager (C), chosen "for performance reasons", with FFmpeg used only for decode/encode features. — [nginx-vod-module#1508](https://github.com/kaltura/nginx-vod-module/issues/1508)
- BENCH: spawning the ffmpeg process takes 27 ms; spawn + probe + first remuxed segment takes 43–48 ms. ffprobe alone takes 43 ms.

### Inferences
- In-process vs spawning: about 30–50 ms per spawn is fine at session start. It hurts on every seek if each seek restarts a process (the Jellyfin model) and when many short-lived sessions run. A Rust in-process packager makes seek = "compute byte ranges for segment N from the index, read, rewrap". For video stream copy, rewrapping one segment is mostly memcpy plus box writing, likely under 1 ms of CPU per 6 s segment, bound by disk read. This is inferred from the BENCH full-file remux rate of about 1.3 GB/s through ffmpeg.
- Practical Rust plan: write your own minimal MKV cluster/block reader. MKV's EBML is simple, and Jellyfin's own Cues extractor is a few hundred lines. Feed it into an fMP4 writer (muxide, mp4, or a hand-rolled `moof`/`mdat` writer). Copy codecPrivate (avcC/hvcC) from the MKV into the init segment. Keep ffmpeg (spawned, or via ffmpeg-next) only for audio transcode (e.g., TrueHD/DTS → AAC/AC-3/E-AC-3), subtitle burn-in and video transcode.
- gstreamer-rs is another in-process option (gst-plugins-rs has fMP4/CMAF muxers), but it carries a large runtime dependency. I didn't verify its maturity or performance here (see Gaps).

### Gaps
- I didn't verify gstreamer-rs/gst-plugins-rs `fmp4mux`/`cmafmux`/HLS sink maturity or benchmarks (search rate-limited).
- I didn't verify `mp4-atom` (kixelated/MoQ) or other 2025–2026 Rust ISOBMFF crates.
- No published benchmarks of pure-Rust MKV demux throughput vs libavformat.

---

## 4. Keyframe index and cue precomputation at scan time; moov-at-end problems; MKV Cues

### Takeaway
Precompute the keyframe index at scan time: (pts, byte offset, size) for every video keyframe, plus track codec-private data, stored in the DB. For MKV, read it from `Cues`, located via the SeekHead; this is about 13 KB for a 20-min file and sub-millisecond to read when present. Fall back to a packet scan when Cues are missing (≈0.4 s per 20 min via ffprobe in BENCH; far slower over network storage). For MP4, read `stss`/`stco`/`stsz` from `moov`. Non-faststart MP4 (moov at the tail) needs an extra tail fetch and breaks some progressive players. Either expose the precomputed index or serve via remux.

### Cited Findings
- Matroska spec: "Unless Matroska is used as a live stream, it SHOULD contain a Cues element"; "For each video track, each keyframe SHOULD be referenced by a CuePoint element"; audio references may be skipped when video is present. — [Matroska Cues spec](https://www.matroska.org/technical/cues.html)
- mkvmerge's no-cues option: files "can be played back without the cue data, but seeking will probably be imprecise and slower". — [mkvmerge man page](https://manpages.ubuntu.com/manpages/trusty/man1/mkvmerge.1.html) **[older]**
- Some players need clusters referenced in the meta-seek elements; otherwise "seeking… doesn't work properly or takes a lot of time". — [MKVToolNix wiki](https://codeberg.org/mbunkus/mkvtoolnix/wiki/Improving-playback-compatibility-with-players)
- VLC's MKV demuxer "generates its own cues while reading and when seeking" when Cues are missing. — [vlc-commits (2011)](https://mailman.videolan.org/pipermail/vlc-commits/2011-December/011079.html) **[older]**
- Jellyfin added Matroska keyframe extraction (merged 2022-01). It describes "matroska metadata supported (pretty fast)" vs ffprobe-based extraction "(slow)", and frame-based tools as "super slow especially over network storage". It parses SeekHead → Info/Tracks/Cues. — [jellyfin PR #6600](https://github.com/jellyfin/jellyfin/pull/6600) **[older]**
- Jellyfin uses those keyframes only when remuxing video, to build HLS playlists with keyframe-aligned segments. With a transcode, the encoder creates new keyframes ("We should not apply keyframes extractor on transcoding"). — [jellyfin PR #7911](https://github.com/jellyfin/jellyfin/pull/7911) **[older]**; [DynamicHlsPlaylistGenerator.cs](https://github.com/jellyfin/jellyfin/blob/fb763c47/src/Jellyfin.MediaEncoding.Hls/Playlist/DynamicHlsPlaylistGenerator.cs)
- Jellyfin's segmenter cuts at the first keyframe ≥ each multiple of the desired length. Without keyframe data it falls back to equal-length segments. — [DynamicHlsPlaylistGenerator.cs](https://github.com/jellyfin/jellyfin/blob/fb763c47/src/Jellyfin.MediaEncoding.Hls/Playlist/DynamicHlsPlaylistGenerator.cs)
- Jellyfin 10.9 has a "Keyframe Extractor" scheduled task; some users hit failures. — [jellyfin#12176 (2024-06)](https://github.com/jellyfin/jellyfin/issues/12176)
- FFmpeg writes moov at the end by default. With range requests "the player fetches the moov atom from the end first… it starts later". `-c copy -movflags +faststart` fixes this without re-encoding. — [ffmpeg-cookbook, moov atom (2026-05)](https://ffmpeg-cookbook.com/en/articles/moov-atom); [faststart (2026-06)](https://ffmpeg-cookbook.com/en/articles/ffmpeg-faststart-web-playback)
- In ExoPlayer progressive playback, non-faststart MP4s "fail to start, or start and then rebuffer continuously" because ExoPlayer recreates the DataSource per seek and pays a close→reopen per hop. "ffprobe, curl, and AVFoundation-based players range-seek to the tail moov and play the same file fine". — [NuvioTV#3100 (2026-08)](https://github.com/NuvioMedia/NuvioTV/issues/3100)
- Chromium had a regression where an MP4 with moov at the end wouldn't play (Chrome 91). — [Chromium issue 40773831](https://issues.chromium.org/issues/40773831) **[older]**
- BENCH: in an FFmpeg-muxed MKV, the SeekHead is in the first 4 KB and Cues (600 keyframes) is 13 KB at the file tail. Reading it took 0.28 ms with a warm cache. A full ffprobe packet scan for the keyframe index took 394 ms for 20 min of 1080p (about 1.2 GB).

### Inferences
- Scan-time plan (inference):
  1. Parse container headers (EBML/SeekHead/Tracks/Cues, or moov) in-process. This needs about 2–3 reads: head 4–64 KB plus tail Cues or moov.
  2. Store per-video-keyframe (pts, offset) plus codec-private data and a precomputed fMP4 init segment.
  3. Mark "needs packet scan" files (no Cues, or Cues only every N seconds) and index them in a low-priority background job.
  4. Mark non-faststart MP4s. Either serve them via the index (a remux path that knows sample offsets), or tell capable clients to prefetch the tail.
- With the index, HLS playlist generation is pure computation (no file I/O at play time), and a seek to any segment is one indexed read.
- Indexing cost on a 2,000-movie library: Cues-based is about 3 small reads per file, i.e. seconds to minutes even on HDD. A packet scan of every file reads the whole library (tens of TB), so do it only for files lacking Cues.

### Gaps
- No survey data on what fraction of real-world MKVs (e.g., WEB-DL vs remux releases) lack Cues or have sparse Cues.
- I didn't measure cold-HDD or SMB/NFS latency for Cues and moov tail reads.

---

## 5. Pre-buffering tricks: prefetching first segments, trickplay/BIF generation cost, next-episode warmup

### Takeaway
Trickplay or BIF generation is the big scan-time CPU cost. Plex documents 10+ minutes per movie at full decode. Jellyfin's keyframe-only mode is about 110× faster (244 vs 2.2 fps). Always generate keyframe-only, decoding only I-frames, and do it at idle priority. For startup, the cheap wins are keeping the index and init segment hot, warming the first segment and file head/tail for "continue watching" and "next episode" items, and letting clients prefetch. I found little hard data on next-episode warmup benefits.

### Cited Findings
- Plex: creating video preview thumbnails "can take a considerable amount of time and CPU resources. It's not uncommon for a single movie to take 10 minutes or more"; index files are "10-50MB in size for a single library item". — [Plex support, Video Preview Thumbnails](https://support.plex.tv/articles/202197528-video-preview-thumbnails)
- Plex: the process "essentially requires a transcode… can take anywhere from less than a minute to several minutes for a single item". — [Plex support, Why is Plex using CPU](https://support.plex.tv/articles/201697383-why-is-plex-using-my-cpu/)
- A third-party tool says Plex generates on CPU "at a frame every 2 seconds by default", with no GPU option. — [Media Preview Generator](https://mediapreviewgenerator.dev/plex-preview-thumbnails-slow/)
- Jellyfin trickplay: the fps-filter method decodes the whole video. Adding `-skip_frame nokey` made it "roughly 110 times faster (244 vs 2.2 FPS, tested on… Ryzen 7950X3D Eco Mode)". QSV/CUVID decoders don't support skip_frame. Blu-ray has "max 1s keyframe interval, streaming services around 2-4s". — [jellyfin#11336 (2024-04)](https://github.com/jellyfin/jellyfin/issues/11336)
- Merged as `EnableKeyFrameOnlyExtraction` ("Significantly faster, but is not compatible with all decoders"). Jellyfin tiles thumbnails 100 per tile and serves them as an HLS image sequence. — [jellyfin PR #11511 (merged 2024-07)](https://github.com/jellyfin/jellyfin/pull/11511); [jellyfin#11336 discussion](https://github.com/jellyfin/jellyfin/issues/11336)
- Real-world rates: keyframe-only took about "1 movie every two minutes" on a 6-core TrueNAS box (40 TB library, "11 days"). Without keyframe-only it took "a movie an hour or two on an 8 core 2.2 Intel atom". A moderator: GPU trickplay is usually slower than CPU. — [Jellyfin forum (2025-04)](https://forum.jellyfin.org/t-trickplay-too-slow-what-will-help)
- On a weak NAS, about 20 s per trickplay image on 1 thread for 5-hour videos. ffmpeg gets killed if an image takes longer than `ImageExtractionTimeoutMs` (default about 20 s) under Idle priority. — [jellyfin#13116](https://github.com/jellyfin/jellyfin/issues/13116)
- nginx-vod-module recommends a large metadata cache "in the order of GBs" so segment requests don't re-read media metadata, plus open-file caching and async file open. — [nginx-vod-module README](https://github.com/kaltura/nginx-vod-module/)

### Inferences
- With a scan-time keyframe index, trickplay can skip FFmpeg's demux overhead: read only the keyframe packets by offset and decode each I-frame. That's about 1 decode per interval instead of a full-stream decode. Speed is then bounded by the number of I-frames decoded × decode cost, plus random reads. On HDD, about 600–3,600 seeks per movie at 5–15 ms each means a few seconds to about a minute per movie. This is inferred, not measured.
- Next-episode/continue-watching warmup (inference):
  - Server: for the likely next items, prefetch into the page cache, or call `posix_fadvise(WILLNEED)` on, the head (first ~1–4 MB), the Cues/moov tail, and the resume-point cluster. Keep the fMP4 init segment and playlist precomputed.
  - Client: once the current item is in its last ~2–5 minutes, fetch the next item's play descriptor and first segment or first N MB. This removes disk spin-up and seek latency, which matters most on HDDs that spin down. A spun-down HDD can take multiple seconds to start (general knowledge, not sourced here).
- The thumbnail sprite format can follow Jellyfin's (tiles of 10×10 JPEGs plus an interval) or Roku BIF. It's cheap to serve either way.

### Gaps
- No measured data on next-episode prefetch benefits in Plex, Jellyfin or Infuse.
- No sourced numbers for HDD spin-up delays or SMB first-read latency in media-server contexts.

---

## 6. What the client needs from the server API to start instantly (one round trip vs several)

### Takeaway
Existing servers need about 3–5 sequential requests before the first media byte:
- Jellyfin: PlaybackInfo POST, then master.m3u8, then main.m3u8, then init, then the segment.
- Plex: decision, then start.m3u8, then index.m3u8, then the segment.

Some steps start processes or run tests (bitrate probes) along the way. The fastest design returns everything needed in **one** "play" response: the decision, the direct URL or an inline media playlist, the init segment URL, and the resume-point segment index. Media bytes then start on the second request, or the response can push or link the first segment.

### Cited Findings
- Jellyfin's resume flow (10.8.13) requested master.m3u8, then main.m3u8, then `0.ts`, then cancelled `1.ts`, then requested the resume segment, with two ffmpeg runs. — [jellyfin#11025](https://github.com/jellyfin/jellyfin/issues/11025)
- Swiftfin slow starts were attributed partly to the "Auto Bitrate Test" running before playback ("waiting on the bitrate test to resolve") and to client TLS and mDNS issues. — [Swiftfin#1444](https://github.com/jellyfin/Swiftfin/issues/1444)
- Plex universal transcode: `/video/:/transcode/universal/decision`, then `start.m3u8`, then `session/<id>/base/index.m3u8`, then segments. A `mediaBufferSize` parameter is passed by clients. — [plex-api crate](https://docs.rs/plex-api/latest/src/plex_api/server/transcode/session.rs.html); [plexclip crate](https://docs.rs/crate/plexclip/latest/source/src/plex/transcode.rs); [plexphp SDK docs](https://github.com/LukeHagar/plexphp/blob/main/docs/sdks/video/README.md)
- Jellyfin: "The client will send transcoding profile(s) to the server containing information about its capabilities… The server will then pick the settings". — [Jellyfin docs](https://jellyfin.org/docs/general/post-install/transcoding/)
- Bitmovin and Mux both identify the chain of network requests (DNS, TCP, TLS, manifest, init, segments) as the main VST contributor. — [Bitmovin Community](https://community.bitmovin.com/t/best-practices-for-optimizing-video-start-time-vst/1655); [Mux](https://www.mux.com/articles/video-analytics-for-developers-guide)
- Apple HLS requires audio and video playlists to share a target duration and segment boundaries. Separate audio renditions add playlist requests. — [Apple HLS authoring spec](https://developer.apple.com/documentation/http-live-streaming/hls-authoring-specification-for-apple-devices)

### Inferences
- Single-round-trip play API (proposal):
  1. The client caches its capability profile on the server once, at registration. Don't re-send it on every play.
  2. `GET /play/{item}?t=resume` returns one of two things:
     - `mode: direct`: the file URL, container, the byte range of the index (Cues/moov) and the head, and the resume byte offset (nearest keyframe ≤ t). A capable client can then issue parallel range requests for head and resume cluster immediately.
     - `mode: hls`: an inline media playlist, or a URL that is pure computation from the precomputed index; the init segment, which is small (BENCH: about 1.3 KB) and can be base64-inlined or at least pre-generated; and the index of the segment containing t.
  3. No process start, no probe and no bitrate test on the critical path. Measure bandwidth passively from the first segment downloads instead.
- Use HTTP/2 or HTTP/3 with keep-alive. Reuse the API connection for media, use IP addresses or cached DNS in clients (the mDNS delay found in Swiftfin), and TLS session resumption. On a LAN this makes the 2 requests cost about 1–2 ms total.
- Use a single muxed fMP4 (video + one audio) per segment for the default path. Separate audio renditions add playlist fetches before start.
- For distributed multi-server setups, the play response from whichever node holds the file should include the direct node URL, so the client doesn't route media through a coordinator.

### Gaps
- No measured comparison of 1-round-trip vs multi-round-trip startup in existing media servers.
- I didn't check whether AVPlayer accepts data-URI or inline media playlists and init segments for VOD. That must be verified before relying on inlining for Apple clients.
