# On-demand transcoding for a distributed home media server (late 2026)

Scope: hardware encoders, HDR tone mapping, engine choice from Rust, just-in-time and distributed transcoding, audio/subtitle-forced transcodes, and pre-transcoding versus on demand. Numbers come first and every number has a source. Data from before 2024 is marked **[older]**. Several 2026 "guide" sites are aggregators; where a figure comes from one, that is noted.

---

## 1. Hardware encoders: concurrent 4K HDR→1080p SDR streams, start latency, quality per bit

### Takeaway
Per watt and per dollar, Intel QSV (iGPU or Arc) and Apple VideoToolbox are the best transcode nodes: they have no session cap, a full zero-copy pipeline and about 3 real-time 4K60 HDR→1080p streams on a laptop-class iGPU (about 9 at 24 fps). NVIDIA scales further on big cards, but GeForce cards share a driver cap per system (8 sessions in SDK 13.0 docs, 12 in SDK 13.1 docs). At equal quality, hardware encoders cost about 1–4 VMAF points, or roughly 5–30% more bitrate, than good software encodes. HW AV1 at about −40% versus x264 is the most efficient real-time option where clients can decode it.

### Cited Findings

**NVIDIA NVENC (session limits and throughput)**
- NVIDIA's NVENC Application Note for Video Codec SDK **13.1**: on "non-qualified" (GeForce) GPUs, concurrent encode sessions are "limited to **12 per system**", combined across all GeForce cards. Qualified (Quadro/RTX Ada pro/Tesla/L4) GPUs are limited only by hardware and memory — [NVIDIA SDK 13.1 app note](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvenc-application-note/index.html)
- The **SDK 13.0** version of the same note says "limited to **8** per system" — [NVIDIA SDK 13.0 app note](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-application-note). A Sept 2025 Plex forum thread confirms 8 at that time — [Plex forum](https://forums.plex.tv/t/latest-affordable-gpu-covering-all-encoders-decoders/930324). A hosting provider read NVIDIA's support matrix on 22 Oct 2026: GTX 1080 Ti, RTX 3090 and RTX 5090 are all "12", and L4/L40S/Tesla T4 are "Unrestricted". A100/H100 have **no NVENC** — [serverroom.net](https://serverroom.net/use-cases/ffmpeg-gpu-transcoding). **Conflict/evolution:** the cap went 8 → 12 with the 13.1 docs. Design for 12 per *machine*, not per card.
- Decodes have no preset limit — [Plex forum](https://forums.plex.tv/t/latest-affordable-gpu-covering-all-encoders-decoders/930324)
- Indicative H.264 encode fps **per NVENC engine** (P1, CBR, low-latency): Turing 855, Ampere 868, Ada 910, Blackwell 977. Multiply by the engine count for aggregate throughput across several sessions. A *single* session cannot exceed one engine unless Split Frame Encoding is on (SDK 12+, Ada and later, HEVC/AV1 only). Context switching between sessions is "negligible" — [NVIDIA SDK 13.1 app note](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvenc-application-note/index.html)
- NVENC engine counts: RTX 3090 has 1; GTX 1080 Ti 2; RTX 5090 3; L4 2; L40S 3 — [serverroom.net](https://serverroom.net/use-cases/ffmpeg-gpu-transcoding)
- Blackwell adds 10-bit H.264 (High10) and 4:2:2 encode — [NVIDIA SDK 13.1 app note](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.1/nvenc-application-note/index.html)
- VRAM per session: about 250–400 MB for 1080p and 1.5–2 GB for 4K (practitioner estimate). An RTX 4070 Ti hits the session cap well before VRAM runs out — [XDA, Aug 2026](https://www.xda-developers.com/old-intel-chip-transcodes-more-jellyfin-streams-than-my-graphics-card-ever-could)

**Intel QSV (iGPU, Arc)**
- "Unlike NVIDIA NVENC, there is **no concurrent encoding sessions limit** on Intel iGPU and ARC dGPU". Runs headless — [Jellyfin Intel docs](https://jellyfin.org/docs/general/administration/hardware-acceleration/intel/)
- Measured Oct 2026, Jellyfin 12.1 + jellyfin-ffmpeg 8.1.3, Jellyfin's own generated commands, 60 fps sources; a stream counts only if the slowest copy stays ≥ source fps — [computingforgeeks, Oct 2026](https://computingforgeeks.com/best-gpu-jellyfin-plex-transcoding/):
  - **Iris Xe (i7-1165G7)**: 4K60 HEVC 10-bit HDR10 → 1080p H.264 8 Mbps, tone mapped: **3 streams (218 fps total)**. Four streams drop to 55 fps each. The ~220 fps budget is about **9 × 24 fps films** on paper. 4K60 AV1 HDR: 3 streams (231 fps). 1080p60 H.264 → 720p: 10 streams (646 fps)
  - HD 530 (6th gen): **0** HDR streams (no HEVC 10-bit decode, software fallback ran at 40 fps)
  - Tone mapping on Iris Xe: **VPP peaked at 194 fps vs OpenCL 220 fps**. Both give 3×60 fps streams. VPP uses less power
  - An 8-stream HDR test ran out of RAM in a 4 GB container (OOM killer), so RAM is a real limit
- SpaceinvaderOne leaderboard (community-submitted, same jellyfin-ffmpeg, 24 fps clips), as quoted by the same article: **4K HEVC→1080p SDR**: Arc A380 **9**, RTX 3050 6GB **5**, Arrow Lake desktop iGPU (DDR5) **12**. **HDR test (which tone maps the full 4K frame first, then scales; Jellyfin does it the other way round)**: Intel iGPU/Arc A-series median **3**, N100-class **2**, Arc B580 **8**, RTX 3050 **5**, RTX 3060 12GB **11**, RTX 2000 Ada **15** (16 SDR). On that test, 4K-first tone mapping is Intel's bottleneck — [computingforgeeks](https://computingforgeeks.com/best-gpu-jellyfin-plex-transcoding/) (leaderboard: [gpu.spaceinvader.one](https://gpu.spaceinvader.one/))
- Arc: disabling Resizable BAR costs about **10%** transcode performance — [Jellyfin hardware selection](https://jellyfin.org/docs/general/administration/hardware-selection)
- Arc B580 (Dec 2024) delivers about **2×** the AV1 encode throughput of the A770 — [Fora Soft](https://www.forasoft.com/learn/video-encoding/articles/hardware-acceleration-nvenc-vpu-asic)
- Aggregator estimates of power and streams: N100 iGPU about 6 W idle and ~3–4 4K streams; Arc A380 about 45 W and ~5–6; RTX 3060 about 170 W and ~12+ — [corelab.tech, Apr 2025](https://corelab.tech/transcoding-architecture-nvenc-vs-quicksync/) (rough guidance, not a controlled measurement)

**Apple VideoToolbox (M-series)**
- "Even an entry-level **M1 can handle three 4K 24fps Dolby Vision HEVC 10-bit transcoding tasks simultaneously while performing tone-mapping to SDR**" (prefer-speed preset; a soft limit). **Max** chips have an extra encode engine that VideoToolbox uses even for one session, enough for 4K120 transcode plus tone mapping. **Ultra** chips have **2 decode + 4 encode engines**, about 2× Max — [Jellyfin Apple docs](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/apple)
- AV1 hardware *decode* starts with M3. VideoToolbox has no AV1 *encode* — [Jellyfin Apple docs](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/apple); [remio.net, May 2026](https://remio.net/blog/hardware-encoder-comparison)
- Real Jellyfin 10.11 command on an M1 (anemone research): `-hwaccel videotoolbox -hwaccel_output_format videotoolbox_vld … h264_videotoolbox -prio_speed 1 … scale_vt … -hls_time 3`. That is a full zero-copy decode→scale→encode pipeline. The project schedules "3–4 × 1080p on M1, more on M4 Pro" per agent (their planning estimate, not a benchmark) — [jellyfin-anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- **[older, 2022]** M1 Pro: 4K `hevc_videotoolbox` encode at default settings ran at 52 fps (1.72× real time) but looked "hazy". Default quality is poor without setting bitrate/quality — [yre.jp](https://yre.jp/en/post/m1mac_h265)

**AMD AMF / VAAPI**
- On Linux, **VA-API is preferred** (open Mesa stack, full acceleration on Polaris+). AMF on Linux is "not recommended, limited support, hardware encoder only, closed source". AV1 encode: RX 7000 / Ryzen 7000+ APUs. HEVC 8-bit encode: Polaris+ — [Jellyfin AMD docs](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/amd)
- AMD tone mapping (OpenCL/Vulkan) "needed extra setup… test it with a real HDR file before promising 4K HDR" — [bigiron.cc, Sept 2026](https://www.bigiron.cc/guides/jellyfin-hardware-transcoding-setup-qsv-nvenc-vaapi-that-works)
- Plex's HEVC encoding option: "AMD is not supported" — [Plex Transcoder support article, modified Sept 2026](https://support.plex.tv/articles/transcoder/)
- RDNA 4 (RX 9070, Mar 2025) added AV1 B-frames. AMD *claims* +25% low-latency H.264 quality and +11% HEVC over the previous generation — [Fora Soft encoder benchmark, citing TechSpot](https://www.forasoft.com/learn/video-quality/articles-vqm/encoder-comparison-x264-x265-svt-av1)

**Rockchip RK3588 (ARM SBC)**
- Jellyfin supports **full acceleration** on the RK3588/3588S VPU via RKMPP (MPP decode/encode + RGA scale/convert/subtitle, zero-copy). Linux only, Jellyfin 10.9+ — [Jellyfin Rockchip docs](https://github.com/jellyfin/jellyfin.org/blob/master/docs/general/post-install/transcoding/hardware-acceleration/rockchip.md)
- Turing Pi RK1 (RK3588, Jul 2026): hardware transcode **4K HEVC Main10 → H.264 at 311 fps (5.18× real time)**. CPU-only 4K HEVC Main10 → 540p used 727% CPU at 39 fps (0.65×). The docs claim the VPU can sustain 1080p@480 fps or 4K@120 fps encode. HDR tone mapping needs the Mali OpenCL runtime (`libmali`) — [Turing Pi](https://turingpi.com/jellyfin-rk3588-hardware-transcoding-turing-pi-arm64)

**Quality per bitrate (VMAF / BD-rate)**
- Fora Soft benchmark (June 2026, updated Oct 2026; VMAF v0.6.1, 1080p, one machine, described as "illustrative house results"). BD-rate versus x264 medium, with encode speed in fps: **NVENC H.264 p7 +5.0% @600**, **NVENC HEVC p7 −30% @500**, **NVENC AV1 p7 −40% @480**, **AMF AV1 −33% @450**, **QSV AV1 (TU1) −38% @400**, x264 veryfast +18% @240, x265 medium −38% @22, SVT-AV1 p8 −51% @30, p6 −55% @11 — [Fora Soft](https://www.forasoft.com/learn/video-quality/articles-vqm/encoder-comparison-x264-x265-svt-av1)
- Blackwell (RTX 5090) HEVC measured 5.59% bitrate saving over the RTX 4080 Super at equal VMAF (Codec Calamity, Feb 2025, via Fora Soft) — [Fora Soft](https://www.forasoft.com/learn/video-quality/articles-vqm/encoder-comparison-x264-x265-svt-av1)
- NVIDIA **[older, 2023]**: NVENC AV1 gives about 40% bitrate savings over NVENC H.264 at 1080p60, with a 1.5–2 dB higher PSNR — [NVIDIA blog](https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture)
- Typical loss from hardware encoding: "one to four VMAF points at the same bitrate" — [Fora Soft HW acceleration article](https://www.forasoft.com/learn/video-encoding/articles/hardware-acceleration-nvenc-vpu-asic)
- Blog *estimate* compiled from public 2023–2025 comparisons: NVENC H.264 (Ada) within ~5–10% of x264 medium bitrate; NVENC HEVC needs ~15–25% more bits than x265 slow; QSV H.264 +5–15%; QSV AV1 "close to NVENC AV1" — [BlazingCDN, Oct 2026](https://blog.blazingcdn.com/en-us/hardware-vs-software-video-encoding-nvenc-qsv-and-x264)
- **[older, Jan 2024]** In an RTX 4060 mobile test, AMF AV1 was "still noticeably behind" NVENC AV1 — [Gough Lui](https://goughlui.com/2024/01/07/video-codec-round-up-2023-part-15-av1_nvencqp-mode-av1-nvidia-nvenc)
- Plex: "Hardware acceleration… can also reduce video quality and compatibility". The CPU quality presets don't affect HW streams — [Plex Transcoder](https://support.plex.tv/articles/transcoder/)

### Inferences
- **Per-device capacity for the scheduler (4K HDR → 1080p SDR, 24 fps film, Jellyfin-style scale-then-tonemap):** M1 ≈ 3 (vendor docs, DV); Iris Xe / desktop Intel iGPU ≈ 3 at 60 fps and ~6–9 at 24 fps; Arc A380 ≈ 3 HDR (4K-first leaderboard) to ~9 (SDR / scale-first); Arc B580 ≈ 8; RTX 3060 ≈ 11; RK3588 ≈ 2–5 (311 fps single-stream throughput suggests ~5×24 fps of headroom, but no measured multi-stream count was found); N100 ≈ 2. GeForce cards are capped at 12 (or 8 on older drivers) per *host*, whatever their throughput.
- Capacity should be *measured fps budget ÷ (source fps × output count)*, not job count. anemone's lesson #7 says the same ("load-balance on real capacity (HW encode sessions in use)").
- Hardware AV1 is the best quality-per-bit live encoder (about −40% vs x264), but only Intel Arc/Arrow Lake+, NVIDIA RTX 40+ and AMD RX 7000+ can encode it, and many clients can't decode it. HEVC HW (about −30%) is the sensible default for capable clients, with H.264 HW as the universal fallback.
- A "fast transcode start" depends mostly on the probe and first segment, not on encoder throughput. Every HW encoder above reaches ≥5× real time on 1080p output, so the first 3 s segment should be encode-bound for only roughly 0.1–0.6 s.

### Gaps
- No controlled 2024–2026 VMAF study was found that compares *VideoToolbox* (M3/M4) against NVENC/QSV at matched bitrate. Remio ranks them only "perceptually".
- No measured time-to-first-segment numbers per vendor (HW init + probe + first 3 s segment) were found. Measure it ourselves.
- No measured wall watts per stream for M-series, Arc or RK3588 under multi-stream transcode were found.
- No measured multi-stream count for RK3588, and no M4/M4 Pro/M4 Max counts, were found.
- The RTX 50-series NVENC session cap (12) comes from a third party's reading of NVIDIA's matrix, not from the matrix page itself.

---

## 2. HDR tone mapping cost and Dolby Vision handling

### Takeaway
Tone mapping is the most expensive stage of a 4K HDR transcode. Done on the CPU it is ruinous (a Ryzen 9 5950X is needed for real-time 4K60 DV→SDR). On a GPU it is cheap if you **scale to 1080p first and tone map a quarter of the pixels**. Use OpenCL (Intel/RK), CUDA (NVIDIA) or Metal (Apple) where Dolby Vision Profile 5 support is needed. Use fixed-function VPP or VideoToolbox-native tone mapping for lower power on HDR10/HLG.

### Cited Findings
- CPU only: tone mapping 4K60 DV → 4K60 H.264 SDR needs a **Ryzen 9 5950X** for faster than real time. Without a GPU, "a Ryzen 9 5950X cannot handle even a single video stream" in some setups — [Jellyfin hardware selection](https://jellyfin.org/docs/general/administration/hardware-selection)
- "Even a basic Intel iGPU can handle as much load as a Ryzen 5800X for this use case" — [Jellyfin transcoding docs](https://jellyfin.org/docs/general/post-install/transcoding/)
- Jellyfin cannot do HDR→HDR or pass HDR metadata through on transcodes. HDR sources must be tone mapped to SDR when transcoded — [Jellyfin transcoding docs](https://jellyfin.org/docs/general/post-install/transcoding/)
- DV **Profile 5 and 8** → SDR tone mapping has been supported since Jellyfin 10.8 with jellyfin-ffmpeg 5.0.1-5+. Intel VPP HDR10 tone mapping works on QSV and VA-API on Linux, and VPP is preferred when both are enabled — [Jellyfin HWA overview](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/)
- Methods per vendor:
  - **Intel**: OpenCL (supports DV P5, fine-tuning, but the runtime sometimes needs a manual install) vs QSV VPP (fixed-function). Supported on any Intel GPU with HEVC 10-bit decode — [Jellyfin Intel docs](https://jellyfin.org/docs/general/administration/hardware-acceleration/intel/)
  - **NVIDIA**: CUDA only, supports DV P5 and zero-copy. DV requires the "enhanced NVDEC decoder" — [Jellyfin NVIDIA docs](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/nvidia)
  - **Apple**: Metal (DV P5, tunable, slower on low-end GPUs) vs **VideoToolbox native** ("lower power consumption, less dependency on GPU performance", but no DV P5). With both enabled, VT native is used and Metal only for P5 — [Jellyfin Apple docs](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/apple)
  - **Rockchip**: OpenCL via Mali `libmali` — [Turing Pi](https://turingpi.com/jellyfin-rk3588-hardware-transcoding-turing-pi-arm64)
- Measured: Iris Xe **OpenCL 220 fps vs VPP 194 fps** (both give 3×4K60 streams); VPP uses less power — [computingforgeeks](https://computingforgeeks.com/best-gpu-jellyfin-plex-transcoding/)
- Order matters: a benchmark that tone maps at 4K then scales cut the A380 from 9 SDR streams to 3 HDR streams, while NVIDIA's HDR counts matched its SDR counts. Jellyfin's scale-first pipeline tone maps one quarter of the pixels — [computingforgeeks](https://computingforgeeks.com/best-gpu-jellyfin-plex-transcoding/)
- libplacebo (Vulkan) can tone map DV Profile 5 to SDR correctly in ffmpeg (Emby community, Dec 2025) — [Emby forum](https://emby.media/community/topic/144597-using-libplacebo-to-fix-the-dolby-vision-profile-5-transcoding-tonemapping-issue/). Jellyfin 12.0 has a Vulkan/libplacebo path; a Sept 2026 bug says `TonemappingPeak` isn't passed to it, which suggests the path is still young (search-result snippet only).
- Plex: transcoded HDR "will appear highly dimmed and desaturated" without tone mapping. HW tone mapping requires Plex Pass and may need extra driver components — [Plex Transcoder](https://support.plex.tv/articles/transcoder/)

### Inferences
- **Pipeline rule for our engine:** HW decode → HW scale to target → tone map (GPU) → HW encode, with all frames kept in GPU memory. Never tone map at 4K when the output is 1080p.
- DV handling: Profile 8.x and 7 have an HDR10-compatible base layer (Profile 7 is the UHD Blu-ray profile), so treat them as HDR10 and drop the RPU/EL. Profile 5 (IPTPQc2, no fallback layer) **needs** a DV-aware tone mapper: OpenCL/CUDA/Metal in jellyfin-ffmpeg, or libplacebo. Tag P5 sources at scan time so the scheduler only routes them to capable nodes.
- Prefer fixed-function tone mapping (Intel VPP, VT native) for HDR10/HLG on low-power nodes. Shader tone mapping (OpenCL/CUDA/Metal/libplacebo) is the general fallback.

### Gaps
- No measured per-frame cost (ms) of libplacebo versus OpenCL `tonemap_opencl` versus CUDA at 1080p was found.
- The Profile 7 handling statement relies on general knowledge plus a Reddit snippet; no primary Jellyfin doc on P7 was retrieved.

---

## 3. Engine choice from Rust

### Takeaway
Spawn a **pinned jellyfin-ffmpeg build** as a supervised subprocess (one per stream) and generate its argv from a typed capability model. jellyfin-ffmpeg's 98 patches (pause/resume keys, `tonemapx`, `scale_vt`, RKMPP, DV) and its process isolation (one crash ≠ server crash) beat in-process bindings. In-process libav (rsmpeg / ffmpeg-next) is worth it only for probing and thumbnails. GStreamer is viable but has weaker HDR/DV and Jellyfin-tested HW paths.

### Cited Findings
- Jellyfin requires jellyfin-ffmpeg for full acceleration: "Using FFmpeg binaries downloaded from somewhere else will result in partial acceleration" — [Jellyfin HWA overview](https://jellyfin.org/docs/general/post-install/transcoding/hardware-acceleration/)
- jellyfin-ffmpeg has **98 patches**. Official portable builds exist for linux64, linuxarm64, mac64, macarm64, win64 and winarm64 (latest v7.1.4-3 at the time of writing). Stock ffmpeg **silently no-ops Jellyfin's throttle**: pause/resume `p`/`u` on stdin is jellyfin-ffmpeg patch 0028 — [jellyfin-anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md). (A different source reports jellyfin-ffmpeg 8.1.3 shipping with Jellyfin 12.1 in Oct 2026 — [computingforgeeks](https://computingforgeeks.com/best-gpu-jellyfin-plex-transcoding/).)
- Jellyfin's process contract (verified in 10.11 source): stdin and stderr redirected; stop = write `q`, wait 5 s, then `Kill()`; throttle = write `p`/`u` every 5 s check; progress parsed from stderr (`fps=`, `time=`); after start the server polls every 100 ms until the output exists; segment N is served once N+1 exists. On startup Jellyfin validates the binary with about 13 probes (`-encoders`, `-filters`, `-hwaccels`, a real lavfi encode, and so on) — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- The 10.11 command line uses `-analyzeduration 200M -probesize 1G` — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md). (Large probe sizes add start latency on network storage. Our engine should reuse probe data captured at scan time.)
- The anemone agent itself is a **Rust daemon that runs the official jellyfin-ffmpeg build** and supervises it over a WebSocket — [jellyfin-anemone](https://github.com/calibrae/jellyfin-anemone)
- Rust crate landscape (Jul 2026), from an author who also promotes ez-ffmpeg (possible bias) — [dev.to](https://dev.to/yeauty/the-state-of-media-processing-in-rust-2026-what-each-crate-actually-covers-2k2c):
  - `ffmpeg-next`: 1.28M downloads/month, "maintenance-only mode", FFmpeg 3.4–8.0, ~40 lines of decode ceremony; fork lineage ffmpeg → ffmpeg-next → ffmpeg-the-third
  - `rsmpeg` (larksuite): tracks FFmpeg 6/7/8 (0.18.0 added 8.0), slow release cadence
  - `ez-ffmpeg`: in-process runtime modeled on fftools; its CLI facade doesn't cover hardware acceleration or complex filtergraphs; experimental, effectively a single maintainer
  - `gstreamer-rs`: ~564K downloads/month, actively maintained, a different pipeline model
  - Subprocess: "A crashed transcode job that cannot take down your service is a feature of the subprocess model"
- ffmpeg-next's crates.io page confirms maintenance mode and FFmpeg 3.4–8.0 support — [crates.io](https://crates.io/crates/ffmpeg-next)
- GStreamer vs FFmpeg: FFmpeg "wins on operational simplicity"; GStreamer is more natural when everything stays in one process via appsink — [muratdemirci.com.tr, May 2026](https://muratdemirci.com.tr/en/ffmpeg-gstreamer/)
- Translating ffmpeg argv between hardware vendors is fragile: "every prior project that tried argv-mangling gave up"; heterogeneous nodes need argv *regeneration* — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)

### Inferences
- Subprocess startup overhead (fork/exec plus HW device init) is in the tens of milliseconds and is dwarfed by probing and first-segment latency, so in-process gains little for the main transcode path. The one real benefit of in-process code is skipping re-probing, which a subprocess can also get by passing known stream info and small `-probesize`/`-analyzeduration` values.
- Build a Rust "argv compiler": (source media facts from the scan DB) × (node capability profile: vendor, codecs, tonemap method, DV support) × (client profile) → argv. Pin one jellyfin-ffmpeg version per release and fingerprint `ffmpeg -version` on node registration.
- Use rsmpeg (or ffprobe as a subprocess) for scan-time probing, keyframe indexing and thumbnail extraction. Rust-native audio (symphonia) lacks AC-3/TrueHD/DTS, so it is not useful here.

### Gaps
- No measured comparison of time-to-first-frame for subprocess ffmpeg versus in-process libav versus GStreamer was found.
- No data on GStreamer's Dolby Vision handling or VA/QSV tone-mapping parity was found.

---

## 4. Just-in-time segment transcoding, throttling, and distributing across machines

### Takeaway
The standard pattern is **whole session → one node**, with HLS segments emitted at forced keyframes every 3 s, throttled (paused) when far ahead of the player, and killed and restarted with `-ss`/`-start_number` on a seek outside the produced range. Real-time playback only needs ~1× speed, so splitting one stream across several machines adds complexity (keyframe/audio-sync artifacts, VRAM duplication) for no viewer benefit. Distribution should happen **per session** (place each stream on the best node) and **per chunk only for offline pre-transcodes**. Every existing distributed transcoder except jellyfin-anemone requires shared storage. anemone uses node-local media mounts plus HTTP PUT of segments.

### Cited Findings

**Throttling / JIT**
- Plex "Transcoder default throttle buffer": "Amount in seconds to buffer before throttling back the transcoder speed" — [Plex Transcoder](https://support.plex.tv/articles/transcoder/). The default is 60 s — [WebTools-NG wiki](https://github.com/WebTools-NG/WebTools-NG/wiki/transcoder). Community advice is to raise it to ≥240 s — [r/PleX](https://www.reddit.com/r/PleX/comments/tkdk4m/pro_tip_increase_your_throttle_buffer_to_at_least/)
- Jellyfin: `TranscodingThrottler` pauses ffmpeg once the buffer ahead of the download position exceeds `ThrottleDelaySeconds` (default **180 s**). `TranscodingSegmentCleaner` deletes segments older than `SegmentKeepSeconds` (default **720 s**). The throttler's first check is at **5 s** and the cleaner's at **20 s**, and throttling needs the client to have downloaded ≥1 segment — [jellyfin#16608](https://github.com/jellyfin/jellyfin/issues/16608)
- Pitfall: remux/direct-stream jobs run at **240–1392 fps (10–58× real time)** on an i5-10600 with no HW, and finish before the throttler fires. Jellyfin caps stream-copy reads at `-readrate 10` when segment deletion is on. A maintainer says ffmpeg ≤7's readrate catch-up is hard-coded and segment deletion stays off by default because "it's difficult to finely control ffmpeg's reading speed through the ffmpeg CLI" — [jellyfin#16608](https://github.com/jellyfin/jellyfin/issues/16608)
- Jellyfin segment mechanics: `-force_key_frames "expr:gte(t,n_forced*3)"`, `-hls_time 3`, mpegts (transcode) or fMP4 (remux) segments, `-copyts`. A seek to a segment that isn't on disk kills the job and restarts ffmpeg with `-ss` + `-start_number N` — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)

**Distributed transcoders (how they work, what breaks)**
- **rffmpeg** (~1068★): Python wrapper that replaces ffmpeg and runs `ssh host ffmpeg …`. It needs NFS for media **and** the transcode dir at identical paths, plus identical jellyfin-ffmpeg and hardware everywhere. Known failures: pause/resume over the SSH PTY (#76), no kill propagation (#89), SQLite locks at 4+ simultaneous starts, and NFS `actimeo` causing **15–60 s segment lag** — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md); [rffmpeg](https://github.com/joshuaboniface/rffmpeg). Jellyfin 10.10+ also needs `TMPDIR` on shared storage — [rffmpeg README](https://github.com/joshuaboniface/rffmpeg). Worker images must match the Jellyfin version exactly — [rffmpeg-worker chart](https://geekxflood.github.io/helm-charts/charts/rffmpeg-worker)
- **ClusterPlex** (~592★): a shim replaces "Plex Transcoder", a Node orchestrator sends jobs to workers over websockets, and workers report back through a local NGINX relay. Needs RWX shared storage for media and `/transcode` at identical paths — [ClusterPlex](https://github.com/pabloromeo/clusterplex). Lesson: Plex SIGKILLs the shim, which was fixed with an explicit kill RPC on socket disconnect — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- **kube-plex**: one pod per transcode on a shared RWX PVC. It died when Plex changed its invocation contract — [kube-plex](https://github.com/munnerz/kube-plex); [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- **UnicornTranscoder**: the worker becomes the client-facing origin (HTTP 307). Unmaintained — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- **jellyfin-anemone** (2026): a C# plugin replaces `ITranscodeManager`. Rust `polyp` agents read media from their own mount (or over HTTP Range from the server) and run jellyfin-ffmpeg with `-f hls -method PUT -http_persistent 1` to push segments back. The control channel is one WebSocket per agent (job, stdin `q/p/u`, stderr, exit, heartbeat). It falls back locally if the agent dies before the first segment. Burn-in subtitles and tone mapping stay local in v0 — [jellyfin-anemone](https://github.com/calibrae/jellyfin-anemone); [RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
  - ffmpeg network caveats: ffmpeg **never reads the HTTP status of a PUT** (a 500 is invisible); `-tls_verify` doesn't reach HLS uploads; `-hls_flags temp_file` is ignored over HTTP, so the receiver must do `.part` + rename; `-f segment` always POSTs and drops headers — [RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
  - Lessons: "**Whole session → one worker. Never split a file across workers (rejected twice upstream: VRAM, keyframe artifacts)**"; kill must be a control-plane message driven by connection liveness; forward stdin bytes unbuffered; use the same ffmpeg build everywhere; local transcoding is the permanent fallback — [RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
  - Jellyfin core sketched this design (task API, node-local input mount, node pushes its own output) in jellyfin-meta #36 (2023) and never built it — [RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- **Tdarr**: distributed library (batch) transcoding with server + nodes and CPU/GPU workers, and flows that hand files between workers — [Tdarr issue](https://github.com/HaveAGitGat/tdarr/issues/1358). One user reports saving ~42 TB with GTX 1060/1070/RX560 workers (anecdote) — [r/Tdarr](https://www.reddit.com/r/Tdarr/comments/s8uybc/thank_you_uhaveagitgat_for_a_great_tool/)
- **Chunked (segment-parallel) encoding**: split at I-frames with `-c copy -an`. Strip audio from the chunks because "when audio is added to each chunk it changes the timestamps… you'll find a nasty drift in your A/V sync". Concat video, then mux the audio back once. It is "very I/O heavy" across nodes — [rustyguts, 2023] **[older]** [link](https://rustyguts.dev/posts/basics-of-chunked-video-coding)

### Inferences
- **Design for our system (no central server, any node transcodes for any node):**
  1. The serving node (closest to the client) owns the session and the HLS playlist. It picks a transcode node by measured fps headroom, codec/tonemap/DV capability, and the network path to the media.
  2. The transcode node reads the source directly (its own mount, or an HTTP Range stream from the owning node) and pushes fMP4 segments over HTTP/QUIC to the serving node, or serves them directly. Our own Rust receiver should verify status codes and checksums, since ffmpeg won't.
  3. Fixed-duration segments with forced keyframes (2–4 s) and deterministic segment boundaries (segment N = [N·d, (N+1)·d)) let a seek, failover or migration restart on *any* node with `-ss N·d -start_number N` and stay byte-compatible in the timeline. This is the only form of "segment parallelism" worth doing live: prefetch-transcode a few segments around a seek target on a second node while the primary catches up.
  4. Throttle with pause/resume (jellyfin-ffmpeg `p`/`u`, or SIGSTOP/SIGCONT on stock ffmpeg) at about 60–180 s ahead. Kill when the client is idle.
- True parallel split-encoding of one live stream across machines only helps when one node can't reach 1× (for example a CPU-only node doing 4K HDR). It costs keyframe and A/V-sync complexity and duplicates decoder state/VRAM. Use it only for offline pre-transcodes (Tdarr-style), where it gives a near-linear wall-clock speedup at the price of network I/O.

### Gaps
- No published numbers for the latency gain from prefetch-transcoding after a seek, or for multi-node live split encoding.
- No measured time-to-first-segment for rffmpeg or anemone (anemone lists it as a spike to measure).
- Unmanic was not researched in depth (no source retrieved).

---

## 5. Audio-only transcodes (TrueHD/DTS → AAC/Opus/EAC3) and PGS burn-in

### Takeaway
Audio-only "Direct Stream" (copy video + transcode audio) is cheap. In a reported Jellyfin case, ffmpeg ran 10–58× real time on a modest CPU without HW, and the main risk was outrunning the throttler. Image subtitles (PGS/VobSub) are the expensive case: if the client can't render them they force a **full video transcode** for burn-in, and the overlay may run on the CPU (Plex) unless the pipeline does hardware overlay.

### Cited Findings
- Jellyfin playback types in order of load: Direct Play < Remux < **Direct Stream (transcodes audio, video untouched)** < Transcode — [Jellyfin transcoding docs](https://jellyfin.org/docs/general/post-install/transcoding/)
- A DirectStream job (H.264 copy + HE-AAC 5.1 → libfdk_aac) ran at **~240 fps (≈10× real time)**, capped by `-readrate 10`, and remux jobs reached **1392 fps (58×)** on an i5-10600 with no HW acceleration — [jellyfin#16608](https://github.com/jellyfin/jellyfin/issues/16608)
- On a Raspberry Pi, Direct Stream/Remux (for example converting 7.1 TrueHD) runs at under 15% CPU (blog claim) — [Stackademic, Sept 2026](https://blog.stackademic.com/building-an-ultra-efficient-home-media-server-on-a-raspberry-pi-with-jellyfin-8399c40439e0)
- On macOS, Jellyfin uses `aac_at` (AudioToolbox) for audio in its transcode command — [anemone RESEARCH.md](https://github.com/calibrae/jellyfin-anemone/blob/main/RESEARCH.md)
- PGS/VobSub "cannot be overlaid by most clients. Jellyfin must burn them into the video stream, which triggers a full transcode" — [jellywatch, May 2026](https://jellywatch.app/blog/jellyfin-direct-play-vs-transcoding-explained-2026). Some clients transcode unnecessarily for PGS (Tizen) — [jellyfin-tizen#202](https://github.com/jellyfin/jellyfin-tizen/issues/202) **[older, 2023]**
- Plex subtitle burn-in "only uses 1 thread" (Aug 2024 test-build thread) — [Plex forum](https://forums.plex.tv/t/subtitle-burn-in-improvement-test-build/884481?page=4). A 2023 r/PleX thread says Plex's burn-in "doesn't run on GPU like on Jellyfin" **[older]** — [r/PleX](https://www.reddit.com/r/PleX/comments/15hw6q6/plex_vs_jellyfin_tone_mapping_subtitles_and_cpu/)
- RK3588's RGA handles "subtitle rendering" in the zero-copy pipeline — [Turing Pi](https://turingpi.com/jellyfin-rk3588-hardware-transcoding-turing-pi-arm64)
- Burn-in with `subtitles=…:fontsdir=` needs the server's attachments/fonts directory, so anemone keeps burn-in on the local node in v0 — [anemone](https://github.com/calibrae/jellyfin-anemone)
- A common workaround is converting PGS to SRT (OCR) so clients render text subtitles — [suptosrt](https://suptosrt.com/pgs-to-srt/jellyfin/)

### Inferences
- For audio-only, transcode on the node that already has the bytes. It is not worth distributing. Prefer EAC3 (5.1 passthrough to most TVs/soundbars) or AAC stereo/5.1 for broad support, and Opus for web clients. Pre-generate a compatible audio track at scan time for TrueHD/DTS-HD titles. The storage cost is tiny (640 kbps EAC3 ≈ 0.29 GB/hour) and it removes a whole class of on-demand jobs.
- For PGS: (a) send the bitmap subtitles to clients that can render them (native players, many web players via libpgs-style renderers); (b) otherwise burn in with a **hardware overlay** (`overlay_qsv`, `overlay_cuda`, `overlay_vaapi`, RGA) so that the burn-in transcode costs the same as a normal one; (c) OCR to WebVTT at scan time as the best long-term fix. Burn-in jobs must ship the subtitle stream (or the extracted .sup) to the remote transcode node.

### Gaps
- No measured CPU cost per stream for TrueHD/DTS-HD MA decode + AAC/EAC3 encode was found. In principle it is a small fraction of one core, but this is unsourced.
- No measured fps penalty for HW versus CPU overlay of PGS in jellyfin-ffmpeg was found.

---

## 6. Pre-transcoding (optimized versions) versus on demand

### Takeaway
Pre-transcode selectively: a 1080p H.264/HEVC "universal" version for titles that are often watched remotely or have DV P5/PGS/TrueHD, and audio-only compatibility tracks for everything with lossless audio. Keep on-demand HW transcoding as the default. Storage is the cost (≈3.6 GB/hour at 8 Mbps) and the payoff is zero start latency and zero GPU load. Software AV1/HEVC pre-encodes beat hardware on quality-per-bit by 15–18 BD-rate points but are 40–400× slower.

### Cited Findings
- Plex Media Optimizer: "Create optimized versions ahead of time so transcoding isn't required on the fly", useful with limited upstream bandwidth — [Plex Media Optimizer overview](https://support.plex.tv/articles/214079318-media-optimizer-overview/). The "Optimized for TV" preset is 1080p @ 8 Mbps, AVC only — [r/PleX](https://www.reddit.com/r/PleX/comments/i48mcu/is_it_recommended_to_store_multiple_versions_of/)
- Plex background transcodes (Optimizer/Downloads) use the x264 preset **"Very fast" by default**. There is an experimental "Enable HEVC Optimization" (HW only) — [Plex Transcoder](https://support.plex.tv/articles/transcoder/)
- Quality versus speed for offline encodes: SVT-AV1 p6 = −55% vs x264 medium at 11 fps (1080p); NVENC AV1 = −40% at 480 fps; x265 medium −38% at 22 fps. For a 10-minute 1080p30 clip, NVENC AV1 takes 37.5 s and SVT-AV1 p4 takes ~67 min. "Encoding cost is paid once, and bandwidth saving is harvested forever" — [Fora Soft](https://www.forasoft.com/learn/video-quality/articles-vqm/encoder-comparison-x264-x265-svt-av1)
- Tdarr users run library-wide HEVC conversion for storage savings (a 42 TB anecdote) — [r/Tdarr](https://www.reddit.com/r/Tdarr/comments/s8uybc/thank_you_uhaveagitgat_for_a_great_tool/)
- "The cheapest and highest-quality transcode is the one you never perform" — client direct-play capability matters more than GPU choice — [aicybr, Aug 2026](https://aicybr.com/blog/jellyfin-hardware-transcoding-intel-nvidia-amd)

### Inferences
- Storage math (derived): 1080p H.264 at 8 Mbps ≈ 3.6 GB/hour; HEVC at the same quality (−30–38%) ≈ 2.3–2.5 GB/hour; a 2-hour film ≈ 5–7 GB extra per optimized version. For a 1,000-film library that is ~5–7 TB, against zero GPU work at play time.
- A home cluster has idle GPUs most of the day, so **idle-time pre-transcoding distributed by chunk across nodes** (split at keyframes, audio separate) uses otherwise wasted capacity. Live transcoding remains session-per-node. Make the policy driven by watch history and client mix (pre-transcode what remote or incompatible clients actually play), with an LRU size cap so it doesn't become a second library.
- Scan-time work worth doing for every file regardless: a keyframe index (for instant seek math and segment boundaries), full ffprobe results (to skip run-time probing), DV profile and HDR type tags, OCR of PGS to text subtitles, and an EAC3/AAC audio companion for TrueHD/DTS.

### Gaps
- No 2024–2026 data on how often home users actually need transcodes (direct-play ratios) was found.
- No measured time-to-first-frame comparison between playing an optimized version and a live HW transcode.

---

## Recommendation summary (for the report writer)

1. **Engine:** a Rust supervisor spawns a pinned **jellyfin-ffmpeg** (one process per session) with argv generated from typed capability profiles. Control goes over stdin (`q`, `p`/`u`) and progress is read from stderr or `-progress`. Probe at scan time and pass small probe sizes at play time.
2. **Hardware priority per node:** Intel QSV (no session cap, VPP/OpenCL tone mapping, AV1 on Arc/Arrow Lake+) ≈ Apple VideoToolbox (lowest power, M1 ≈ 3×4K DV→SDR, Max/Ultra have more engines) > NVIDIA (fastest on big cards, capped at 12/8 sessions per GeForce host) > RK3588 (311 fps single-stream 4K→H.264, Linux only) > AMD VA-API (verify tone mapping per generation). Advertise capacity as a measured fps budget, not a session count.
3. **Pipeline:** zero-copy HW decode → scale to target → GPU tone map (VPP/VT-native for HDR10; OpenCL/CUDA/Metal/libplacebo for DV P5) → HW encode (HEVC for capable clients, H.264 fallback, AV1 where both ends support it) → 2–4 s fMP4 segments with forced keyframes on a fixed timeline.
4. **Distribution:** whole session → one node, chosen by headroom and capability. The node reads media directly and pushes segments over HTTP/QUIC with checksums. Seek, failover and migration are restarts at a segment index. Throttle by pausing at ~60–180 s ahead. Split-by-chunk only for idle-time pre-transcodes.
5. **Avoid forced transcodes:** pre-generate audio companions (EAC3/AAC) and OCR'd text subtitles at scan time. Use a hardware overlay when PGS burn-in is unavoidable.
