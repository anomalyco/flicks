# Compute-heavy "nice" playback features: implementation approaches, cost per hour, accuracy (late 2026)

Scope note: data gathered 30 Oct 2026. Where a number comes from a quick local benchmark run for this research, it is marked **[local bench]**: Apple M5 Max, Homebrew ffmpeg, 1280×720 H.264 30 fps 1.48 Mbit/s 30-minute camera file (low-detail content, so image sizes are a lower bound), and a 32-minute stereo AAC 48 kHz screen recording for audio. These are single runs, not rigorous benchmarks. Vendor and blog sources are marked as such; forum posts are marked "user report".

## 1. Intro / recap / credits detection

### Takeaway
Do it in three cheap tiers: (1) container chapters (almost free), (2) crowd-sourced marker DBs keyed by TVDB/IMDb + S/E (one HTTP call), (3) local Chromaprint over only the first ~10 min of each episode in a season plus keyframe-only black-frame/credits scans near the end. The analysis is cheap (seconds of CPU per episode). The real costs are decoding audio and reading the file from disk. Fingerprints and results are small and can be shared. The 2026 consensus is that a missing skip button is better than a wrong one.

### Cited Findings
**How existing servers do it**
- Plex intro detection: the server "will analyze the beginning of each episode, looking for segments that match between the episodes. This is primarily done by analyzing the audio of the file… Matching is done per season." It needs Plex Pass on the server admin account and on the viewer's account. Skipping jumps to "a couple of seconds before the intro ends". — [Plex Support: Skip TV Show Intros](https://support.plex.tv/articles/skip-content/)
- Plex credits detection covers episodes and movies, and detects mid-/post-credits scenes. "Contributing Marker Data to 'the Cloud'": Plex submits "a 'hash'… generated based on the unique file… along with the marker detection times". **"This feature only works with credits markers currently."** Credits markers can also drive "mark as played" instead of a completion-percentage threshold. — [Plex Support: Credits Detection](https://support.plex.tv/articles/credits-detection/)
- Plex intro detection needs more than one episode, because it compares audio between episodes. — [Plex Forum (user/staff thread)](https://forums.plex.tv/t/intro-and-credit-with-just-1-episode-even-if-i-had-more/935861)
- Emby Intro Skip needs Emby Server 4.7+ and Premiere. Detection runs per season and needs ≥2 episodes. — [Media Preview Generator comparison, citing Emby docs](https://github.com/stevezau/media_preview_generator/blob/dev/docs/comparison.md) / [Emby Intro Skip](https://emby.media/support/articles/Intro-Skip.html)
- Jellyfin has no built-in detection. Since 10.10 it stores typed **Media Segments** (Intro, Outro, Recap, Preview, Commercial) that plugins provide and clients act on. An official "Chapter Segments Provider" plugin derives them from chapter names. — [Jellyfin docs: Media segments](https://jellyfin.org/docs/general/server/metadata/media-segments/)

**intro-skipper (Jellyfin plugin, maintained fork; wiki edited Oct 2026)**
- Methods: **Chapter** ("fastest method… without decoding the whole video", and can use SponsorBlock labels); **Chromaprint** (compares audio between files to find repeated spans); **Black frame** (scans keyframes for black frames, can detect rolling credits on black and credits on "near-uniform low-saturation card"); **Silence** (only refines a boundary). — [intro-skipper wiki Home](https://github.com/intro-skipper/intro-skipper/wiki)
- Analysis window: by default only min(25% of runtime, 10 min) is analysed for intros. Intro 15–120 s; credits 15–450 s (movies up to 900 s); recap and preview 15–120 s. — [intro-skipper wiki: Settings ‐ Analysis](https://github.com/intro-skipper/intro-skipper/wiki/Settings-%E2%80%90-Analysis)
- Black-frame defaults: ≥85% of pixels below brightness 28. "Refine credits boundary" probes between keyframes. You can disable it for "faster, keyframe-only boundaries". Black-frame analysis "is CPU work and is separate from hardware video decoding". — [intro-skipper wiki: Black Frame](https://github.com/intro-skipper/intro-skipper/wiki)
- Caching design: it stores an analysis record per item and segment type, with a config hash and the media-file version, plus reusable "Chromaprint fingerprints and keyframe scans". Changing boundary or duration settings does not re-decode media. In Jellyfin 12, the plugin DB is the source of truth and Jellyfin MediaSegments is a "synchronized mirror". Manual edits are durable, and deleting an auto segment creates a suppression. — [intro-skipper wiki Home](https://github.com/intro-skipper/intro-skipper/wiki)
- Performance knobs: max parallelism, FFmpeg nice/priority, codec threads per process, scan timeout default 300 s, and compressed fingerprint cache. — [intro-skipper wiki: Performance](https://github.com/intro-skipper/intro-skipper/wiki)
- First-run cost (vendor blog, approximate): 100 episodes 10–30 min; 1,000 episodes 2–6 h; 5,000 episodes 12–24 h. After that, analysis is incremental. Failure cases: cold opens, anime whose OP changes between cours, talk/reality shows, and single episodes. — [JellyWatch blog, Jun 2026](https://jellywatch.app/blog/jellyfin-intro-skipper-setup-configuration-troubleshooting-2026)
- Chromaprint fingerprint resolution: each fingerprint item covers ≈0.1238 s of audio. — [Essentia Chromaprint tutorial](https://essentia.upf.edu/tutorial_fingerprinting_chromaprint.html)

**Third-party / ML-ish approaches**
- Media Preview Generator (MIT, Docker, 2026) detects from four sources: chapters, **online skip databases**, the shared theme song (TV only), and "the credit roll itself, read from the text on screen near the end" (OCR). It detects once per file and publishes to Plex, Jellyfin and Emby. Policy: "If it can't confirm an intro or credits from the file itself, it writes nothing, because a missing skip button is better than one that skips into the story." Weak spots: sports (off by default), broadcast TV with logos or scores, burned-in captions, and movies (little online coverage). — [MPG docs: skip-intro-credits](https://github.com/stevezau/media_preview_generator/blob/dev/docs/skip-intro-credits.md)
- plex-credits-detect uses spectrographic fingerprinting (SoundFingerprinting) across a season to add credits. — [cjmanca/plex-credits-detect](https://github.com/cjmanca/plex-credits-detect)
- ScenePass (Kodi): detecting a recap the first time is "extremely slow", because each episode must be fingerprinted against all previous episodes. — [Kodi forum](https://forum.kodi.tv/showthread.php?tid=388822)

**Crowd-sourced marker databases (they exist and are active in 2026)**
- **TheIntroDB** (theintrodb.org): community-verified intro, recap, credits and preview timestamps for TV and movies. Free API (v4) with an npm package, and plugins for Jellyfin, Emby, Kodi and more. Keyed preferably by **TVDB** id (TMDB and IMDb as fallbacks), sends the series display order, and supports multiple release types. Rate limits: anonymous 30 req/10 s and 5,000 lookups/day; with an API key 120 req/10 s and 100,000/day. The Jellyfin plugin fills segments during the scheduled "Media segment scan", not at play time, and refreshes every 30 days to pick up corrections. — [TheIntroDB Jellyfin plugin README](https://github.com/TheIntroDB/jellyfin-plugin), [TheIntroDB API docs](https://theintrodb.org/docs)
- **IntroDB** (introdb.app): intro, outro and post-credit timestamps. `GET api.introdb.app/segments?imdb_id=…&season=…&episode=…`, "No keys", anonymous reads. Clean submissions go live instantly, and challenged ones go to review. The site says "Backed by Infuse". Intro range 5–180 s. — [introdb.app](https://introdb.app/)
- The Plex cloud credits markers are keyed by file hash (see above) and are not public.

### Inferences
- **Cost per episode is small.** 100 episodes in 10–30 min means ≈6–18 s of wall time per episode on typical hardware. Chromaprint only needs audio from the first ≤10 min (intro) and the last ~5–15 min (credits). Audio decode plus FFT is cheap: compare the full EBU R128 pass in §4, which decodes a whole 32-min stereo AAC track in 1.45 s **[local bench]**. In practice the dominant cost is reading the file and demuxing past the video packets, especially on HDDs and NAS. Per-season pairwise comparison is O(n²) but tiny (fingerprints are ~8 items/s × 600 s ≈ 4.8k uint32 per episode ≈ 20 KB).
- **Recommendation (pipeline order):**
  1. Chapter names (ffprobe, milliseconds).
  2. TheIntroDB/IntroDB lookup by TVDB/IMDb + S/E. **Verify locally**: a DB timestamp is per episode, not per file, and releases differ (PAL speed-up, cold opens, extended cuts). A cheap check is to confirm that the cached Chromaprint of the candidate span matches the season's other episodes.
  3. Chromaprint across the season, intro window ≤10 min.
  4. Credits: scan keyframes for black or low-saturation frames in the final 15 min, optionally with OCR. Refine with silence.
  5. Emit nothing when confidence is low.
- **Distributed sharing:** store (a) the Chromaprint fingerprint blob (~20 KB per episode, compressible) and (b) the final segment list (<200 bytes) keyed by content hash. Another node then never re-decodes. That node can also re-run season matching when a new episode arrives using only the stored fingerprints. This is the same design intro-skipper uses locally (fingerprint cache separate from results).
- Optionally contribute back to TheIntroDB (submission API exists), which raises the household's privacy footprint. Make it opt-in.
- Use credits start as the "Up Next" trigger and as the watched threshold (Plex does this).

### Gaps
- I found no rigorous public accuracy figures (precision/recall) for intro-skipper, Plex or Emby detection. Only qualitative failure modes.
- I found no measured per-episode CPU seconds for Chromaprint itself. fpcalc was not installed locally, and the local ffmpeg lacks the chromaprint muxer. The per-episode estimate above is derived from first-run library timings.
- Coverage size of TheIntroDB/IntroDB (number of episodes) was not retrievable. The sites render counts client-side.
- Plex's exact intro algorithm and whether Plex shares intro markers across users (beyond credits) are not documented.

## 2. Trickplay / scrubbing thumbnails

### Takeaway
Decode keyframes only (`-skip_frame nokey`). It is ~110× faster than full decode in Jellyfin's test and ~7.6× less CPU in the local bench. With it, thumbnails cost about **10 s of one CPU core per hour of 720p H.264** and **~2.5–3 MB per hour** at 320 px/10 s. That is cheap enough to generate at scan time on any idle node and replicate. Lazy on-first-scrub generation is unnecessary for most files.

### Cited Findings
- Jellyfin issue #11336 (Apr 2024): extracting keyframes only is "roughly 110 times faster (244 vs 2.2 FPS… Ryzen 7950X3D)". The command adds `-skip_frame nokey` before the input and keeps the fps filter for correct timestamps. Blu-ray keyframe interval ≤1 s; streaming sources 2–4 s. Accuracy suffers only below ~10 s intervals. A Jellyfin dev notes that **QSV and CUVID decoders do not support `skip_frame`**, so the gain applies to software decoders and some hwaccels. Merged as an option in PR #11511 (10.10). — [jellyfin/jellyfin#11336](https://github.com/jellyfin/jellyfin/issues/11336), [Jellyfin 10.10.0 release](https://jellyfin.org/posts/jellyfin-release-10.10.0/)
- Jellyfin trickplay format: thumbnails packed into tiles (default 10×10 = 100 thumbs per JPEG tile), served via an HLS-style image playlist. Tile duration = interval × count. — [same issue, gnattu comment](https://github.com/jellyfin/jellyfin/issues/11336)
- Jellyfin defaults: interval 10 s, width 320 px, 10×10 tiles, `ProcessThreads`=1, priority BelowNormal. HW decode, HW MJPEG encode and keyframe-only are **off** by default. HW MJPEG encode is limited to QSV, VA-API, VideoToolbox and RKMPP. — [MPG Jellyfin trickplay doc](https://github.com/stevezau/media_preview_generator/blob/dev/docs/jellyfin-trickplay-gpu.md), [MPG comparison citing TrickplayOptions.cs](https://github.com/stevezau/media_preview_generator/blob/dev/docs/comparison.md)
- Jellyfin trickplay storage: user report of 320 px 10×10 averaging 0.75 MB per (15–20 min) video, which works out to ≈2.3–3 MB/h. — [r/jellyfin (user report)](https://www.reddit.com/r/jellyfin/comments/1ptwzcs/trickplay_storage_need/)
- Plex BIF ("media index files"): "A typical index will be 10–50 MB in size for a single library item". Default interval is 2 s (`GenerateBIFFrameInterval`). `GenerateBIFKeyframesOnly` defaults on and "greatly speeds up processing and reduces CPU usage". Plex calls it "essentially… a transcode", CPU-only, "less than a minute to several minutes for a single item", and "hours or even days" for big imports. — [Plex Support: Video Preview Thumbnails](https://support.plex.tv/articles/202197528-video-preview-thumbnails/), [MPG summary of Plex docs](https://github.com/stevezau/media_preview_generator/blob/dev/docs/plex-preview-thumbnails-slow.md)
- Media Preview Generator: GPU decode (NVIDIA, Intel, AMD on Linux) with CPU fallback. Decodes each file **once** and writes Plex BIF, Emby BIF and Jellyfin tiles. Supports worker groups with weekly hours and remote worker nodes. Triggered per file by Sonarr/Radarr webhooks. Default interval 10 s. Warns that "If your disks are the bottleneck, a GPU won't fix that". An Emby plugin dev's view is that I/O dominates. — [MPG comparison](https://github.com/stevezau/media_preview_generator/blob/dev/docs/comparison.md)
- HDR/DV thumbnails need tone-mapping. Plex users report washed-out HDR and green Dolby Vision. Jellyfin tone-maps only if transcode tone-mapping is on. — [MPG comparison](https://github.com/stevezau/media_preview_generator/blob/dev/docs/comparison.md)
- **[local bench]** 30 min 720p H.264, 320 px, 1 frame/10 s, MJPEG q4:
  - keyframe-only (`-skip_frame nokey`): **4.64 s wall, 4.65 s user** (≈9.3 CPU-s per hour of video).
  - full software decode + fps filter: 7.06 s wall, **35.3 s user** (≈70 CPU-s per hour).
  - full decode via VideoToolbox: 56 s wall (slower here, probably because of hwdownload and per-frame overhead at 30 fps).
  - Output: 180 JPEGs = **1.44 MB per 30 min (≈2.9 MB/h)**.

### Inferences
- **Recommendation:** at scan time on an idle node, run software keyframe-only decode, downscale to 320 px (plus an optional 160 px tier for phones), one frame per 10 s (or per keyframe when GOP > 10 s), and pack into WebP or JPEG tile sheets of 10×10. Store as content-hash-keyed blobs of ~3 MB/h (a 4K remux would be larger per decoded keyframe in CPU, roughly constant in output size). A 1,000-hour library is ~3 GB of thumbnails at 10 s, versus Plex's 2 s BIFs at ~5× the frame count (10–50 MB per item).
- HW decode is mostly pointless here. Keyframe-only software decode beats HW full decode, and QSV/CUVID can't skip non-keyframes. Use the GPU only for HEVC/AV1 4K keyframes on weak CPUs, or for HDR tone-mapping.
- Lazy on-first-scrub generation adds latency the moment the user scrubs, and keyframe-only takes seconds per hour of video. A hybrid is reasonable: generate at ingest for new items, and backfill old items in idle windows. If a client scrubs an item without tiles, serve on-demand single keyframe seeks (`-ss` before the input plus one keyframe decode, ~tens of ms) as the fallback.
- Replication: tile sheets are small, immutable and content-addressed. Replicate to the nodes that serve the file. Clients cache them.

### Gaps
- No 4K HEVC / AV1 keyframe-only timing found. Keyframe decode cost scales with resolution, so expect it to be several times the 720p number. Not measured.
- No authoritative Plex BIF resolution and JPEG quality documentation was found.

## 3. Subtitles: OCR of image subs, download, sync, speech-to-text

### Takeaway
Priority order:
1. Text subs already in the container (free).
2. OpenSubtitles download by moviehash: exact-file matches need no sync. The free tier is only 5–20 downloads/day/IP, so cache and share across the household.
3. alass or ffsubsync sync for name-matched subs: ~15–30 s per film.
4. OCR of PGS/VobSub via Tesseract/PaddleOCR (RapidOCR): minutes, CPU-only.
5. Generated subtitles with Parakeet TDT 0.6B v3 (≈60–155× realtime on Apple Silicon, under 1 min per hour) or Whisper large-v3-turbo (≈7–18× realtime on Apple Silicon, ≈35× on an RTX 4090), on a node with an NPU/GPU, never a NAS CPU.

### Cited Findings
**OpenSubtitles (download)**
- Downloads: anonymous 5/day; free account 20/day; VIP 1,000/24 h. Searching is unlimited, only downloads are restricted. Pro or premium API packages exist. — [OpenSubtitles help: About the API](https://opensubtitles.tawk.help/article/about-the-api), [Getting Started](https://opensubtitles.tawk.help/article/getting-started), [OpenSubtitles blog (VIP 1000/24 h)](https://blog.opensubtitles.com/opensubtitles/saying-goodbye-to-opensubtitles-org-api-embrace-the-20-black-friday-treat)
- Request rate: "5 requests per second per IP. It is not a concurrency limit or a daily total" (os_admin). The paid "Basic plan" has 50 rps and 15k downloads per 24 h. Responses carry `x-ratelimit-remaining-second`. On quota exhaustion the API returns 406. — [OpenSubtitles forum: quotas for free app](https://forum.opensubtitles.com/t/proper-flow-and-quotas-for-integration-into-free-app/8038), [forum: download rate limit](https://forum.opensubtitles.com/t/download-rate-limit/6415), [bazarr#2153](https://github.com/morpheus65535/bazarr/issues/2153)
- Requires an `Api-Key` header plus a User-Agent. The `/download` response includes `remaining` downloads, and results carry `moviehash_match`. The legacy opensubtitles.org XML-RPC API was shut down for non-VIP users. — [OpenSubtitles blog: subtitle finder CLI](https://blog.opensubtitles.com/opensubtitles/how-to-build-a-movie-subtitle-finder-cli-with-python-and-the-opensubtitles-api), [forum: .org shutdown](https://forum.opensubtitles.com/t/opensubtitles-org-api-final-shutdown-notice-for-non-vip-users/5045)
- **OSHash / moviehash** = file_size + Σ uint64-LE(first 64 KB) + Σ uint64-LE(last 64 KB). It reads only 128 KB per file. Files with the same size and the same head and tail collide. — [opensubtitles/oshash](https://github.com/opensubtitles/oshash)

**Sync**
- alass (Rust, language-agnostic, handles ad-break splits and framerate differences): audio extraction "about 10 to 20 seconds", alignment "5 to 10 seconds". 88–98% of subtitles come out "good". Random OpenSubtitles.org subtitles had about a 50% error rate before sync (N=118). After sync, 50% of lines were within 50 ms, 80% within 100 ms, 90% within 400 ms, 95% within 800 ms. `--no-splits` mode is "very fast". Can sync against a reference subtitle instead of audio. *(README numbers date from ~2019; older data.)* — [kaegi/alass README](https://github.com/kaegi/alass)
- ffsubsync (Python): VAD options WebRTC (default), auditok, and `fused` (WebRTC + Silero). Supports framerate-ratio correction, `--max-offset-seconds` (default 60), `--split-penalty`, `--max-duration-seconds N`, and `--multi-segment-sync` (8 sampled segments by default) to avoid decoding the whole track. — [smacke/ffsubsync README](https://github.com/smacke/ffsubsync)

**Image-subtitle OCR**
- pgsrip: extracts PGS with MKVToolNix, OCRs with **Tesseract**, or falls back to **RapidOCR (PP-OCRv6 small)**, and downloads language data automatically. — [ratoaq2/pgsrip README](https://github.com/ratoaq2/pgsrip)
- Subtitle Edit: nOCR is "the most accurate but it has to be trained" (user report). Binary Image Compare is fast, but per-font training is needed. One guide calls Tesseract inside Subtitle Edit "painfully slow". — [r/SubtitleEdit (user report)](https://www.reddit.com/r/SubtitleEdit/comments/1v809he/most_accurate_ocr_engine/), [SE issue #3315](https://github.com/SubtitleEdit/subtitleedit/issues/3315), [iamscum OCR guide (undated, likely older)](https://iamscum.wordpress.com/guides/ocr/)

**Speech-to-text generation**
- Parakeet TDT 0.6B v3 on Apple Silicon: ≈60× realtime (58× podcast, 63× meeting, own measurement), with 103× on a 5-min podcast on an M4 Pro. Average WER 6.32% vs Whisper large-v3-turbo 7.83% on the Open ASR Leaderboard (English). — [WhisperNotes blog (vendor)](https://whispernotes.app/blog/parakeet-v3-default-mac-model)
- MacParakeet: "A 60-minute file processes in about 23 seconds on M1". Parakeet v3 is about 155–237× realtime on the Neural Engine path. — [macparakeet.com blog (vendor)](https://macparakeet.com/blog/whisper-to-parakeet-neural-engine/)
- NVIDIA reports Parakeet 0.6B TDT RTFx ≈2,940 on the HF Open ASR leaderboard (datacenter GPU, batched). — [NVIDIA NGC](https://catalog.ngc.nvidia.com/orgs/nvidia/collections/parakeet-tdt-0.6b)
- whisper.cpp + CoreML (ANE), large-v3-turbo Q5_0: RTF ≈0.14 (about 7× realtime, so ≈8.5 min per hour of audio). — [neosophie blog, Feb 2026](https://neosophie.com/en/blog/20260218-local-whisper)
- Whisper on Apple Silicon (aggregator): large-v3 runs 5–14× realtime (M1 to M5 Pro), and large-v3-turbo 14–18× realtime on an M5 base. — [PromptQuorum (aggregator; lower confidence)](https://www.promptquorum.com/local-llms/apple-silicon-whisper-metal-benchmark)
- RTX 4090, faster-whisper int8: large-v3-turbo **35.2× realtime** (1.5 GB VRAM); large-v3 19.7×. "An hour of audio takes roughly one to four minutes" on GPU. — [Runpod 2026 guide](https://www.runpod.io/articles/guides/best-gpu-for-whisper)
- faster-whisper large-v3-turbo WER 9.5% sequential and 7.7% batched in one benchmark. — [SYSTRAN/faster-whisper#1030](https://github.com/SYSTRAN/faster-whisper/issues/1030)

### Inferences
- **Recommendation:**
  - Compute OSHash at scan time. It costs 128 KB of I/O and gives a free dedupe key across the household and an exact-subtitle lookup.
  - Search freely, but gate downloads through one household-wide quota manager: 20/day/IP free, or the user's own VIP key with 1,000/day. Cache every downloaded subtitle as shared metadata keyed by (oshash, lang).
  - Always run alass in `--no-splits` mode first, then fall back to split mode when the result is not a moviehash match. Pick alass (Rust, embeddable as a crate) over ffsubsync (Python).
  - Compute the VAD/speech-activity timeline once per file (cost ≈ audio decode) and store it as compact metadata. Every later sync for any language can then skip decoding.
- STT cost per hour of video:
  - Parakeet v3 MLX/ANE: ~23–60 s on M-series. It is the best default for English and European languages.
  - Whisper large-v3-turbo: ~3.5–8.5 min on Apple Silicon, ~1.7 min on an RTX 4090. Use it for other languages and translation.
  - On an N100-class CPU, expect it to be impractically slow. No source found; schedule STT only on nodes with ANE/GPU capability.
- Image-sub OCR: prefer RapidOCR/PaddleOCR (or Apple Vision on macOS nodes) over Tesseract. Run it only when there is no text track in the target language. PGS frames are sparse, so OCR workload scales with line count (~1,000–1,500 events per film), not runtime.

### Gaps
- No quantitative CER/WER for PGS→SRT OCR (Tesseract vs PaddleOCR vs nOCR) from 2024–2026 was found.
- OpenSubtitles terms on caching or redistributing downloaded subtitles among multiple servers were not found. Check the API ToS before replicating cached subs.
- No modern benchmark of alass vs ffsubsync speed and accuracy on the same corpus.
- Parakeet v3 language coverage was not confirmed from a primary source.

## 4. Audio: loudness normalization, dialogue boost / night mode

### Takeaway
EBU R128 / loudnorm analysis is very cheap: about 3–5 CPU-s per hour of stereo AAC **[local bench]**. Do it at scan time and store per-track numbers (integrated loudness, true peak, LRA, ~40 bytes). Plex applies normalization and "Boost Dialog" by **transcoding audio on the server**. A lighter design applies the gain (ReplayGain-style) and a dynamic-range compressor or dialogue EQ on the client, keeping direct play.

### Cited Findings
- Plex "Audio Track enhancements" (PMS 1.43.4+, clients from 2025.28 / Roku 9.0.31 / Web 4.162.1):
  - **Boost Dialog**: "the server isolates and boosts the frequency that dialog occurs at".
  - **Normalize Loudness**: needs per-track analysis ("Analyze audio tracks for loudness": Never / scheduled / scheduled + on add).
  - Both need Plex Pass, and "**Enabling either of these options will cause the audio to be transcoded**". Analysed streams expose `canNormalizeLoudness="1"`. — [Plex Support, last modified 1 Oct 2026](https://support.plex.tv/articles/audio-track-enhancements-for-video/)
- Plex's measurement as observed in PMS 1.43.4: `ffmpeg -i FILE -map 0:TRACK -af loudnorm=I=-16:TP=-1:LRA=9:print_format=json -f null -`. Plex stores `ln:loudness`, `ln:peak`, `ln:lra`, `ln:threshold`, `ln:gainOffset` and `ln:loudnessAnalysisVersion` in the stream's `extra_data`. "Decoding audio gains nothing from a GPU." — [Media Preview Generator: Plex loudness doc](https://github.com/stevezau/media_preview_generator/blob/dev/docs/plex-loudness-normalization.md)
- **[local bench]** ffmpeg `ebur128` over a 32.3-min stereo AAC 48 kHz track: **1.45 s wall, 2.38 s user** (≈2.7 s wall / ≈4.4 CPU-s per hour of audio). Result: I = −23.1 LUFS, LRA 11.1 LU.

### Inferences
- **Recommendation:** at scan time, run R128 (integrated, LRA, true peak; a Rust `ebur128` crate is available) on every audio track and store it as a few floats per track. Optionally add a coarse loudness or dialog-band envelope (e.g. 1 value per 400 ms is ~9 KB/h) to drive smarter client-side night mode.
- Client side: apply a static gain to the −16…−24 LUFS target, capped by true peak. For "night mode", apply a compressor or limiter in the client's audio pipeline (AVAudioEngine, ExoPlayer audio processors, WebAudio DynamicsCompressor). For "dialogue boost", downmix with a raised centre channel (5.1) or a 1–4 kHz presence EQ. This keeps direct play and avoids a server audio transcode, unlike Plex.
- Multichannel lossless tracks (TrueHD, DTS-HD MA) cost more to decode than AAC, but this is still likely seconds to tens of seconds per hour.

### Gaps
- No measured cost for analysing TrueHD/Atmos or DTS-HD MA tracks.
- Plex's Boost Dialog filter chain is not documented.
- Client-side DRC feasibility per platform (e.g. passthrough to an AVR disables client DSP) needs separate research.

## 5. Chapters and markers from containers; credits start for "Up Next"

### Takeaway
Container chapters are free (one ffprobe/demux of the header). Map named chapters to segments with regexes (intro-skipper and Jellyfin's Chapter Segments Provider both do this). Use the detected credits start, not a fixed percentage, as the "Up Next" and "mark watched" trigger.

### Cited Findings
- Chapter detection "uses chapter markers placed in the file by the encoder. Named chapters can provide start and end points without decoding the whole video. This is the fastest method". SponsorBlock labels are optional. — [intro-skipper wiki](https://github.com/intro-skipper/intro-skipper/wiki)
- intro-skipper: a recognized credits chapter is authoritative by default. "Enhance chapter credits" combines it with black-frame and Chromaprint results to extend credits or find extra blocks. — [intro-skipper wiki: Analysis](https://github.com/intro-skipper/intro-skipper/wiki/Settings-%E2%80%90-Analysis)
- Jellyfin's official "Chapter Segments Provider" creates media segments from chapter names. — [Jellyfin docs: Media segments](https://jellyfin.org/docs/general/server/metadata/media-segments/)
- Plex uses credits markers to decide "played" status and shows post-play/Up Next at the final credits marker. It also detects mid- and post-credits scenes and skips to them. — [Plex Support: Credits Detection](https://support.plex.tv/articles/credits-detection/), [Plex blog](https://www.plex.tv/blog/let-the-next-episode-roll/)
- Emby has no credits end, so skipping credits goes to the end of the file, past any post-credits scene. — [MPG skip-intro-credits doc](https://github.com/stevezau/media_preview_generator/blob/dev/docs/skip-intro-credits.md)

### Inferences
- Segment model: {type ∈ intro, recap, preview, commercial, credits, post-credits-scene; start; end; source ∈ chapter, db, chromaprint, blackframe, ocr, user; confidence}. Keep user edits as durable overrides plus suppressions, as intro-skipper does. Replicate segments as tiny CRDT-friendly records keyed by content hash.
- Use "credits start" (high confidence) for Up Next and watched status. Fall back to a percentage only when there is no marker.

### Gaps
- No data on what fraction of typical rips carry named chapters.

## 6. Live TV / DVR: HDHomeRun, M3U/XMLTV, comskip, storage

### Takeaway
HDHomeRun tuners expose an HTTP MPEG-TS stream and a lineup API, so integration is cheap. Recording is a stream copy at roughly **3–8 GB per hour** for ATSC 1.0 MPEG-2. Comskip on a modern x86 takes **2–5 min per recorded hour** (≈20 min/h on a Raspberry Pi 5). ATSC 3.0 is complicated by DRM (unsupported on HDHomeRun) and AC-4 audio.

### Cited Findings
- SiliconDust: "HD content typically runs around 6GB per hour". The vendor recommends ≥500 GB for recordings and wired Ethernet for the DVR device. — [HDHomeRun DVR on Windows](http://info.hdhomerun.com/info/dvr:windows), [HDHomeRun DVR](http://info.hdhomerun.com/info/dvr)
- User report: a 30-min recording at 3.14 GB. — [Plex forum](https://forums.plex.tv/t/files-are-huge/563930)
- Comskip speed:
  - "less than 5 minutes to process 1 hour of an OTA recording… on low priority" (user report). — [Channels community](https://community.getchannels.com/t/how-many-cores-for-comskip-with-4-available/26047)
  - "20 minutes to process a one hour recording on a RPi5", with timestamps "ready a minute or so after the recording has completed" when run live (user report). — [Tvheadend forum](https://tvheadend.org/d/9243-realtime-comskip)
  - A 90-min native .ts took 2 min 2 s. — [electro-dan blog](https://electro-dan.co.uk/blog/46/my-tv-recording-setup)
  - **Older:** ~3 h per 1 h H.264 HD show on a Core 2 Duo E8400. — [SageTV forum, ~2009](http://forums.sagetv.com/forums/showthread_t_38699.html?t=38699)
  - MPEG-2 sources comskip faster than H.264 ("30 min show in under 2 mins"). — [Zatz Not Funny, 2019 (older)](https://zatznotfunny.com/2019-07/channels-tv-everywhere/)
- Lossless commercial removal: tvtrim uses comskip to detect, then ffmpeg stream copy. — [fewtarius/tvtrim](https://github.com/fewtarius/tvtrim)
- ATSC 3.0: "Some ATSC 3.0 channels may be DRM encrypted and will not work… the HDHomeRun FLEX 4K will use the ATSC 1.0 version of the channel". Broadcasters can block out-of-home viewing, force recordings to expire, or block recording. — [SiliconDust HDHomeRun](https://www.silicondust.com/hdhomerun.html)
- Community tools (2026) proxy HDHomeRun into Jellyfin or Plex and convert AC-4 audio with Jellyfin-FFmpeg. LineDrive transcodes live to 720p for browsers at "about one CPU core per viewer". — [r/jellyfin Lineup 2.0](https://www.reddit.com/r/jellyfin/comments/1wjgxu1/lineup_20_hdhomerun_guide_atsc_30_transcoding_and/), [r/hdhomerun LineDrive](https://www.reddit.com/r/hdhomerun/comments/1xecuyi/linedrive_4_is_live_live_tv_in_the_guide_comskip/)
- One user claims "Plex server does not support ATSC 3.0". This may be outdated. — [AVS Forum (user report; low confidence)](https://www.avsforum.com/threads/atsc-3-0-computer-tuner-card.3192711/page-2)

### Inferences
- **Recommendation:**
  - Record as an untouched TS stream copy (no CPU).
  - Run comskip (or a Rust port of its black-frame, logo and silence heuristics) after recording on the least-busy AC-powered node. Store commercials as `Commercial` segments, not cuts, so they are reversible and tiny.
  - Optionally run a background HEVC/AV1 hardware re-encode to cut storage 2–4×. This is an estimate based on MPEG-2 vs HEVC efficiency, not sourced.
  - For live viewing, serve TS directly to clients that can decode MPEG-2 and AC-3, and transcode only for browsers (≈1 core per viewer).
- M3U/XMLTV ingestion is parsing work (negligible CPU). Normalize to one channel and program model. Replicate the guide as metadata between nodes rather than having every node fetch it.

### Gaps
- No primary-source numbers on XMLTV/M3U parsing cost (assumed trivial).
- No 2025–2026 comskip accuracy figures. Its accuracy depends heavily on the per-channel .ini.
- The share of ATSC 3.0 stations encrypted in 2026 is unclear. A ZapperBox blog snippet mentioned "about 75% of ATSC 3.0 broadcasts", but the sentence was truncated and its meaning could not be confirmed.

## 7. Scheduling background work across nodes; dedupe by file fingerprint

### Takeaway
No mainstream server schedules across household nodes. Media Preview Generator and Tdarr come closest: worker groups and remote nodes with per-group hours and job types. Plex and Jellyfin run everything inside one server on a schedule. A distributed design should key every analysis by a cheap content hash (OSHash-style, 128 KB read) and replicate the small results.

### Cited Findings
- Media Preview Generator:
  - "Named worker groups. Pick each group's devices, job types, worker count and weekly hours."
  - Remote GPUs join as "worker nodes".
  - Each file is decoded once and published to several servers, with a skip when "previews are current".
  - Retries at 1, 2, 5, 15 and 60 min when the server hasn't indexed the file yet.
  - High-priority new imports "jump ahead" of backfills. Loudness jobs default to Low priority and run at nice 10.
  - Before writing, it re-checks file size and the Plex bundle hash, and "A changed source is left unwritten and checked again later." — [MPG README](https://github.com/stevezau/media_preview_generator/blob/dev/README.md), [MPG Plex loudness doc](https://github.com/stevezau/media_preview_generator/blob/dev/docs/plex-loudness-normalization.md), [MPG slow-previews doc](https://github.com/stevezau/media_preview_generator/blob/dev/docs/plex-preview-thumbnails-slow.md)
- Tdarr: a server with multiple nodes. Workers are typed as Transcode CPU/GPU and Health Check CPU/GPU, and worker limits are managed per node. — [Tdarr for Unraid](https://ca.unraid.net/apps/tdarr-0zaoev80x6obfc), [tdarr.io](https://tdarr.io/)
- Jellyfin trickplay runs FFmpeg at BelowNormal priority with 1 thread by default. intro-skipper exposes parallelism, process priority and threads. — [MPG comparison](https://github.com/stevezau/media_preview_generator/blob/dev/docs/comparison.md), [intro-skipper wiki](https://github.com/intro-skipper/intro-skipper/wiki)
- Plex runs thumbnails and loudness during the "scheduled maintenance" window or on add. — [Plex Support](https://support.plex.tv/articles/202197528-video-preview-thumbnails/)
- OSHash reads 128 KB regardless of file size. Collisions happen if size, head and tail match. — [opensubtitles/oshash](https://github.com/opensubtitles/oshash)
- Plex keys cloud credits markers by a hash of the file. — [Plex Credits Detection](https://support.plex.tv/articles/credits-detection/)
- intro-skipper invalidates by "media-file version" plus a config hash, and keeps evidence caches separate from results. — [intro-skipper wiki](https://github.com/intro-skipper/intro-skipper/wiki)

### Inferences
- **Recommended design:**
  - **Key:** `content_id = OSHash(size, head 64 KB, tail 64 KB)` for cheap identity and OpenSubtitles compatibility. Add a stronger lazy hash (e.g. BLAKE3 of a few sampled 1 MB chunks) to guard against collisions and remuxes. Use (path, size, mtime) only as a local change detector.
  - **Results:** `(content_id, analyzer, analyzer_version, params_hash) → result` (segments, fingerprints, R128 floats, VAD timeline, subtitle blobs, tile sheets), replicated as small records. Large blobs such as tiles and subtitles are content-addressed and fetched on demand.
  - **Job claims:** a lease per (content_id, analyzer) with TTL, replicated (e.g. via the same metadata log). The first idle node claims, and others skip.
  - **Placement:** prefer the node holding the file locally (I/O dominates for thumbnails, audio and Chromaprint). Next prefer nodes on AC power and idle (OS idle time, no active playback, thermal state). Next prefer capability: ANE/GPU for STT, CPU for audio. Respect per-node quiet hours.
  - **Priority:** newly added or about-to-be-watched first (e.g. next episode in a series someone is watching), then backfill.
- **Per-hour cost budget** for "all features" on a 1 h 1080p episode, rough, from the findings above:

  | Work | Cost per hour of video |
  |---|---|
  | Thumbnails (keyframe-only) | ~10–30 CPU-s |
  | R128 | ~5 CPU-s |
  | Chromaprint (intro and credits windows) | a few CPU-s plus reading ~1/3 of the file |
  | Black-frame credits scan (keyframes, last 15 min) | a few CPU-s |
  | Sync (alass) | ~15–30 s |
  | STT (only if no subs) | 25–60 s on Parakeet/ANE, or minutes on Whisper |
  | Comskip (DVR only) | 2–5 min |
  | Total (excl. STT) | about ≤1 CPU-minute plus one sequential read of the file |

  So the key optimization is to **fuse analyzers into a single read/demux pass per file** (video keyframes → tiles and black frames; audio → R128, Chromaprint windows and VAD) rather than optimizing each feature separately.

### Gaps
- No existing open-source media server was found that does household-wide, power-aware (AC/battery/idle) placement. The idle and AC heuristics above are design inference, not sourced practice.
- No measured comparison of fused single-pass vs per-feature passes. Worth benchmarking in the prototype.
