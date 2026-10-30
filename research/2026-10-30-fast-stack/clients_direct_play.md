# Client strategy for maximum direct play and fast playback (as of late Oct 2026)

Research note for a Rust, distributed, Plex-like home media server. Sources are mostly 2025–2026; older items are marked. Search access was rate-limited partway through, so some questions are partly answered (see Gaps).

## 1. 2026 codec/container support matrix by platform

### Takeaway
No browser direct-plays a typical "remux" MKV (HEVC 10-bit + DV + TrueHD/DTS + PGS) end to end. Native players do much better: mpv/FFmpeg-based apps on Apple and Android, Kodi, and Roku/LG/Samsung TVs within their spec. The usual breakers are: DTS/TrueHD in browsers and AVPlayer, PGS/ASS subtitles outside mpv/Kodi/Infuse, DV Profile 7, and MKV in Safari/AVPlayer.

### Cited Findings

**Browsers (Jellyfin's maintained table, current master)**
- Video: H.264 8-bit ✅ in Chrome/Edge/Safari (🔶 in Firefox). H.264 10-bit ✅ in Chrome/Edge, ❌ in Firefox, 🔶 in Safari (manual toggle, macOS ≥14 on Apple Silicon). HEVC 8/10-bit: 🔶 in Chrome/Edge/Firefox (Windows needs the Microsoft Store HEVC extension; Firefox on Linux needs system ffmpeg), ✅ in Safari. VP9 ✅ in all four. AV1 ✅ in Chrome/Edge/Firefox, 🔶 in Safari. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support) ([raw md](https://github.com/jellyfin/jellyfin.org/blob/master/docs/general/clients/codec-support.md))
- Audio: AC3/EAC3 ❌ in Chrome, ✅ in Edge, ❌ in Firefox, ✅ in Safari. DTS ❌ in every browser. Opus ✅ in Chrome/Edge/Firefox. Safari Opus needs `.caf`, or MP4 (stereo, iOS 17/macOS 14+), or OGG (iOS 18.4/macOS 15.4+). FLAC ✅ in all. ALAC is Safari only. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)
- Containers: MKV ✅ in Chrome/Edge (footnote: "MKV on Chrome requires at least Chrome 145"). MKV ❌ in Firefox and Safari. Jellyfin has disabled Firefox MKV "as there are significant issues making it unusable" (Mozilla bugs 2000420, 1991746). MP4/WebM/TS are fine everywhere; WebM on Safari needs Safari 14+. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support). Mozilla says "MKV support shipped in [Firefox] 145" — [Mozilla Connect](https://connect.mozilla.org/t5/ideas/add-support-for-matroska-media-container-mkv-files/idi-p/15935/page/4). *Conflict/ambiguity: the Jellyfin footnote may mean Firefox 145 or Chrome 145; I could not confirm a Chrome 145 MKV change.*
- HDR in browsers: Safari has the most complete support (HDR10, DV including P5, HLG). Chrome and Firefox support HDR10 only: Chrome on HEVC/VP9/AV1, Firefox on VP9/AV1 only. On Windows, DV works only in Edge with the Dolby Vision Store add-on. LG webOS HDR works, but "mkv containers are known to cause issues", so Jellyfin recommends a force-remux-to-MP4 option. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)
- Chromium has no HEVC software decoder (GPU decode only), so HEVC fails when hardware acceleration is off. — [Brave Community thread](https://community.brave.app/t/has-something-changed-with-hevc-support/648927). Firefox ≥133 enables HEVC decoding by default on Windows only and "doesn't seem able to render HLG/PQ correctly". — [StaZhu/enable-chromium-hevc-hardware-decoding](https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding)
- Subtitles by container: PGS only in MKV. ASS only in MKV (🔶 in AVI). SRT ✅ in MKV, 🔶 in MP4. Burning in subtitles is "the most intensive CPU component of transcoding". — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)

**Apple (iOS/tvOS): AVPlayer vs mpv in Swiftfin (current `players.md`)**
- Containers: MKV, MPEG-TS and WebM are ✅ on MPV and ❌ on AVPlayer. MOV/MP4 ✅ on both. — [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
- Audio: DTS, DTS-HD, TrueHD and Opus are ✅ on MPV and ❌ on AVPlayer. AAC/AC3/EAC3/ALAC/FLAC ✅ on both. — [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
- Video: AV1 needs an A17 Pro/M3 or newer for acceptable performance. HEVC needs an A8X/M1 or newer. "Apple devices do not currently report hardware VP9 decoding, so AVPlayer will normally transcode VP9". MPEG-2 works on MPV only. MPV only direct plays SDR H.264, and anamorphic video gets transcoded. — [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
- DV/HDR:
  - DV P5, P8.1 and P8.4 are ✅ on both engines.
  - DV P7.6: MPV ❌ (the server tone-maps it); AVPlayer 🔶 ("plays the base layer only and ignores the enhancement layer").
  - HDR10+ plays as HDR10 on both, with dynamic metadata ignored.
  - In MPV's native-DV mode, image or ASS subtitles need server burn-in.
  - Framerate matching on tvOS works only with AVPlayer.
  - [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
- Subtitles: PGS, DVDSub and DVBSub are ✅ on MPV if embedded in MKV (external files need burn-in). On AVPlayer they always need server burn-in. SRT and ASS on AVPlayer are converted by the server to WebVTT inside HLS, which "requires the server to remux or transcode the video" and drops ASS styling. — [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)

**Android / Android TV / Google TV / Fire TV / Shield (platform + Media3)**
- AV1 decoding is in the platform from Android 10; encoder and decoder are mandatory from Android 14. VP9 has been supported since Android 4.4 in WebM/MKV/MP4. Listed HDR formats: HLG10, HDR10, HDR10+, DV 8.4. — [Android supported media formats](https://developer.android.com/media/platform/supported-formats)
- In the Jellyfin table, AV1 on Android TV is 🔶 ("needs at least Android TV 10"). HEVC 10-bit on Android TV is 🔶. Vorbis ❌ on Android TV. DTS ✅. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)
- ExoPlayer/Media3 software decoder extensions (AV1, VP9, FLAC, Opus, FFmpeg, MIDI, IAMF, MPEG-H) "must be manually built and included". — [Media3 supported formats](https://developer.android.com/media/media3/exoplayer/supported-formats). FFmpeg adds DTS, TrueHD, ALAC and PCM variants. Platform decoders cover AAC/MP3/FLAC/Opus/Vorbis/AC-3/E-AC-3. ExoPlayer is Apache-2.0 and adds about 1–3 MB to an APK vs about 20–40 MB for LGPL/GPL libVLC. — [LiveAPI blog (secondary)](https://liveapi.com/blog/exoplayer); [androidx/media #2966](https://github.com/androidx/media/issues/2966) (a community-built FFmpeg AAR with ac3/eac3/dca/truehd, Dec 2025)
- DV Profile 7 on Android TV:
  - Jellyfin Android TV 0.19 started checking device DV support. P7 (`DOVIWithEL`) on devices without full P7 support now triggers a remux/transcode, where it previously direct-played. "Almost all Android TV set-top boxes don't fully support Dolby Vision Profile 7". — [jellyfin-androidtv #5073 (Oct 2025)](https://github.com/jellyfin/jellyfin-androidtv/issues/5073)
  - Plex direct-plays the same P7 + TrueHD Atmos MKV to an NVIDIA Shield, with ExoPlayer falling back to HDR10 on the client. Jellyfin's request to match this was closed as not planned. — [jellyfin-androidtv #5303 (Dec 2025)](https://github.com/jellyfin/jellyfin-androidtv/issues/5303)

**Roku**
- Developer spec:
  - MKV/WebM: video AVC, HEVC; audio AAC, AC3, EAC3, Dolby Atmos, ALAC, DTS, DTSE, FLAC, MP3, OPUS, PCM, VORBIS.
  - MP4: AVC/HEVC with AAC/AC3/EAC3/AC4/ALAC/MP3/PCM.
  - DTS is passthrough only.
  - 4K devices: HEVC up to level 5.1 at 40 Mbps.
  - AV1 is listed as "DASH only" in the streaming table.
  - DV listed as dvhe.05 / dvav.09, plus HDR10 and HDR10+.
  - [Roku streaming specifications](https://developer.roku.com/dev/docs/media)
- Roku Ultra (USB playback): VP9.2 in MKV, AV1 in MP4/MKV, DTS, EAC3/AC-4 Atmos; DV, HDR10/10+, HLG. — [Roku Ultra product page](https://www.roku.com/products/players/roku-ultra)
- In the Jellyfin table, Roku HEVC is "only supported on 4K devices". AC3/EAC3 are native only on Roku TVs and the Ultra, and passthrough on everything else. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)

**LG webOS**
- webOS 6.0 MKV: video MPEG-2/4, H.264, VP8, VP9, HEVC, AV1; audio DD, DD+, AAC, PCM, DTS, MP3. UHD models: HEVC 4K60 Main10 L5.1 up to 60 Mbps, VP9 and AV1 4K60 up to 50 Mbps. 8K models: AV1 8K60 at 40 Mbps. — [webOS TV 6.0 A/V formats](https://webostv.developer.lge.com/develop/specifications/video-audio-60)
- LG's own USB page says "DTS is only supported when playing video files via USB and HDMI" (i.e. not in streaming apps). — [LG USA support](https://www.lg.com/us/support/help-library/lg-tv-supported-video-codecs-for-connected-usb-devices--20153095976198)
- Jellyfin 12.0 added "Dolby Vision in MKV files on webOS 25 and newer". — [Jellyfin 12.0 post](https://jellyfin.org/posts/jellyfin-release-12.0)

**Samsung Tizen**
- 2025 TVs: HEVC Main/Main10 4096×2160@60 up to 100 Mbps; AV1 4K60 at 100 Mbps; VP9 Profile 0/2 in WebM. — [Samsung 2025 TV video specs](https://developer.samsung.com/smarttv/develop/specifications/media-specifications/2025-tv-video-specifications.html)
- "The DTS Audio codec is not supported on 2024 TVs". WMV/VC-1/WMA were dropped on 24TV_BASIC4 and on 2025+ models. — [Samsung 2024 TV video specs](https://developer.samsung.com/smarttv/develop/specifications/media-specifications/2024-tv-video-specifications.html)
- Jellyfin 12.0 added anamorphic direct play on Tizen and AV1 direct streaming on TV clients. — [Jellyfin 12.0 post](https://jellyfin.org/posts/jellyfin-release-12.0)

**Kodi / Jellyfin Media Player (desktop)**
- In the Jellyfin table, Kodi and Jellyfin Media Player are ✅ for every listed video codec (including MPEG-4 Part 2, VP9, AV1, H.264 10-bit) and every listed audio codec (including DTS). — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)
- In Jellyfin for Kodi "Add-on mode", "Kodi will direct play nearly all files and put little overhead on the Jellyfin server". JellyCon is a thin streaming add-on (GPLv2) that suits multiple servers or users better. — [Jellyfin Kodi docs](https://jellyfin.org/docs/general/clients/kodi); [jellycon repo](https://github.com/jellyfin/jellycon)

### Inferences
- A "universal direct-play profile" for the server to aim at is H.264 or HEVC Main10 in MP4/fMP4, with AAC stereo plus E-AC3 5.1 (or Atmos), DV P8.1/P5 or HDR10 metadata, and text subtitles as sidecar WebVTT/SRT. This plays on Safari/AVPlayer, Roku, webOS, Tizen and Android TV. Chrome/Firefox lack AC3/E-AC3, so browsers still need an AAC (or Opus) track; the server can transcode audio cheaply.
- The cheapest server-side fallback is container remux plus audio-only transcode. A video transcode is needed only for VP9/AV1 on old devices, DV P7 on non-mpv clients, and bitmap/ASS subtitle burn-in. Subtitle burn-in is the costly trap; prefer clients that render PGS/ASS themselves.
- DV P7 is best handled by sending the file unchanged and letting the client fall back to the HDR10/base layer (as Plex and AVPlayer do), or by a cheap server-side remux that strips the enhancement layer to P8.1. A tone-mapping transcode is the costly option to avoid.

### Gaps
- I did not get Apple's own tvOS/iOS codec docs (e.g. whether tvOS 26 AVPlayer changed MKV or Opus support). Apple data here comes via Swiftfin's docs.
- I did not get caniuse/MDN numbers for WebCodecs or HEVC-via-MSE share, or a definitive source for "Chrome 145 MKV".
- I found no per-device specs for Google TV Streamer, Fire TV 4K Max (2023) or Shield. Device-level DV P7/FEL support is known only through the issue threads above.
- Roku's AV1 "DASH only" vs AV1-in-MKV on USB: I could not confirm how this applies to the Jellyfin Roku app's progressive playback.

## 2. Which clients direct-play the most, and their transcode triggers

### Takeaway
Ranked by direct-play breadth:
1. Infuse (Apple)
2. Kodi (any platform), roughly tied with mpv-based apps: Swiftfin-MPV, Streamyfin-MPV, Findroid-mpv, Jellyfin Media Player
3. ExoPlayer apps with FFmpeg audio: Jellyfin Android TV
4. TV-native apps on Roku/webOS/Tizen
5. AVPlayer-only apps
6. Browsers

The universal triggers are DV P7 (outside Plex/Infuse/AVPlayer base-layer fallback), PGS/ASS on non-mpv players, DTS/TrueHD in browsers/AVPlayer/Samsung, and MKV in Safari/AVPlayer.

### Cited Findings
- **Infuse 8:**
  - Supported formats include MKV, MP4, ISO, BDMV, VIDEO_TS, AV1, VP9, VC-1, H.266/VVC; audio AC3, E-AC3, TrueHD, DTS, DTS-HD MA, FLAC; subtitles PGS, SSA/ASS, VobSub, DVB, SRT, VTT.
  - Pro adds "Dolby Vision (Profile 5 & 8)", lossless HD audio and E-AC3 Atmos for $1.99/mo, $16.99/yr or $99.99 lifetime.
  - Requires iOS 18+ / tvOS 18.5+ / macOS 15+ / visionOS 26+.
  - [Firecore Infuse page](https://firecore.com/infuse)
- Infuse 8.4.7 (16 June 2026) added Atmos detail info from Jellyfin, and DV metatags from Emby/Jellyfin were added earlier. — [Firecore forum](https://community.firecore.com/t/dolby-atmos-badge-info-for-jellyfin-plex/57109); [Infuse release notes](https://firecore.com/releases)
- A comparison table claims Infuse direct-plays DTS, DTS-HD, PGS, ASS, AVI, AV1, VP9 and "DV Profile 7", while the Plex app transcodes DTS, PGS and ASS and falls back to HDR10 for P7. — [Phlix blog, Sep 2026 (secondary, vendor blog)](https://phlixapp.com/blog/plex-vs-infuse). *This conflicts with Firecore's own "Profile 5 & 8" claim. P7 most likely plays as the HDR10 base layer.*
- **Swiftfin:**
  - Now ships two engines, MPV (default, "recommended for the broadest compatibility") and AVPlayer. Older Jellyfin docs and the README still describe VLCKit, so those tables are stale. — [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md); [Swiftfin README](https://github.com/jellyfin/swiftfin)
  - Swiftfin-MPV transcode triggers: DV P7 (tone-mapped); external bitmap subtitles; bitmap/ASS subtitles while in native-DV mode; anamorphic H.264; non-SDR H.264. AirPlay video is unsupported on MPV.
  - Swiftfin-AVPlayer transcode triggers: MKV/TS/WebM (remux), DTS/TrueHD/Opus, VP9, any text subtitle (HLS remux), any bitmap subtitle (burn-in).
  - [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
  - Issue: the Native/AVPlayer engine caused "2-4x higher server CPU than VLCKit during Direct Play" on tvOS. Closed via PR #2382. — [Swiftfin #2201 (Aug 2026)](https://github.com/jellyfin/Swiftfin/issues/2201)
  - A Swiftfin player bug failed to trigger transcoding when bandwidth limits applied. Fixed in PR #1882. — [Swiftfin #1854](https://github.com/jellyfin/Swiftfin/issues/1854)
- **Streamyfin:** uses MPV via MPVKit "as its primary video player on all platforms". 2026 releases added an opt-in ExoPlayer engine on Android TV with HDR10/HDR10+, DV P5 direct play and Atmos passthrough, plus "AV1 direct play only when the device can". Earlier 2026 builds offered VLC and KSPlayer. About 5.3k GitHub stars (Oct 2026). — [Streamyfin README](https://github.com/streamyfin/streamyfin); [Streamyfin releases](https://github.com/streamyfin/streamyfin/releases); [issue #1451](https://github.com/streamyfin/streamyfin/issues/1451); [Hysen Labs](https://hysenlabs.com/projects/streamyfin-streamyfin)
- **Findroid:** a native Android Jellyfin client with a built-in mpv player ("all media formats will play correctly including styled SSA/ASS subtitles") and offline downloads; 4.6★ from 581 reviews. — [Google Play](https://play.google.com/store/apps/details?id=dev.jdtech.jellyfin&hl=en_US)
- **Jellyfin Android TV:** 0.18 added a "Prefer FFmpeg for audio playback" option (FFmpeg fallback for audio decoding). — [Jellyfin Android TV 0.18](https://jellyfin.org/posts/androidtv-v0.18.0). In 0.19, DV P7 now triggers a remux/transcode on non-P7 devices. — [#5073](https://github.com/jellyfin/jellyfin-androidtv/issues/5073)
- **Jellyfin Android (mobile):** the integrated player gained AC3, E-AC3, TrueHD, DTS and LPCM in v2.4.0 (Nov 2021, older data). — [Jellyfin Android 2.4.0](https://jellyfin.org/posts/android-v2.4.0)
- **Kodi:** "will direct play nearly all files" via Jellyfin for Kodi. — [Jellyfin Kodi docs](https://jellyfin.org/docs/general/clients/kodi)
- **Jellyfin web / iOS app:** HDR works there but "not Swiftfin" per the older docs paragraph, which contradicts the current Swiftfin docs showing DV/HDR on both engines. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support) vs [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
- **Server-side relief in Jellyfin 12.0:**
  - "Image-based subtitles can be rendered by the client during remuxing" (cuts PGS burn-in).
  - A spec-compliant `dvh1` HLS variant for DV P5.
  - [Jellyfin 12.0 post](https://jellyfin.org/posts/jellyfin-release-12.0)

### Inferences
- Recommended client per platform, prioritising direct play:

  | Platform | Pick | Alternatives |
  | --- | --- | --- |
  | Apple TV / iOS | Infuse (paid) | Swiftfin with MPV (free, MPL-2.0) |
  | Android TV / Shield / Google TV | Jellyfin Android TV | Kodi (CoreELEC for DV P7 FEL) for enthusiasts |
  | Android phone | Findroid or Streamyfin (mpv) | |
  | Fire TV | Jellyfin Android TV or Kodi | |
  | Roku | Jellyfin Roku | |
  | LG / Samsung TVs | Jellyfin webOS / Tizen | plan for DTS→AC3/E-AC3 audio transcode on Samsung 2024+ and LG streaming |
  | Desktop | Jellyfin Media Player (mpv) or Kodi | browser as fallback |

- If the server speaks the Jellyfin API, most of these come for free. Infuse is the strongest Apple TV client and is closed source, so compatibility depends on matching Jellyfin's API closely.
- The server must still implement DeviceProfile/PlaybackInfo negotiation honestly. Clients such as Jellyfin Android TV 0.19 now decide transcodes from it, and wrong answers cause either needless transcodes or broken playback.

### Gaps
- I found no 2025–26 source on Plex HTPC (mpv-based) direct-play behaviour or on the current Plex Apple TV player. The Plex claims above come from a secondary blog only.
- I did not verify Infuse's behaviour with DV P7 FEL on Apple TV from Firecore directly.
- I found no source on Jellyfin Roku app transcode triggers in 2026 (e.g. PGS or DTS on non-passthrough setups).

## 3. Jellyfin API surface clients need; reimplementations; Jellyfin 12.0 changes

### Takeaway
The full Jellyfin contract is 412 operations, but a playback-capable client touches far fewer. One Rust reimplementation, Ferrofin (GPL-3.0, Aug 2026), already claims all 412 routes wired, with measured 2–12× latency wins. Jellyfin 12.0 (7 Sep 2026) removed `/emby` and `/mediabrowser` prefixes and disabled legacy auth by default, which narrows what a compatible server must emulate.

### Cited Findings
- **Jellyfin 12.0, released 7 Sep 2026 (tag v12.0, 8 Sep):**
  - Versioning jumps from 10.11 to 12.0 and the server reports `12.0.0`.
  - "Support for the legacy `/emby/` and `/mediabrowser/` addresses has been removed, and the deprecated way of signing in is now disabled, including on existing servers".
  - `GetItems` is async and applies `recursive` when filters plus `includeItemTypes` are given.
  - Swashbuckle v10 changes the generated OpenAPI document, so SDKs must be regenerated.
  - HLS controllers are hidden from the spec.
  - Removed routes: `POST /Users/{userId}/EasyPassword`, `GET /Items/{itemId}/CriticReviews`, `GET /Environment/NetworkShares`, `POST /System/MediaEncoder/Path`, `GET /LiveTv/Recordings/Groups/{groupId}`, and `GET /QuickConnect/Initiate` (use POST).
  - Newly obsolete: `GetTrailers`, `GetArtists`/`GetAlbumArtists` (use `GetPersons`), `UserDto.HasPassword`.
  - Server now targets .NET 10 and FFmpeg 8.1.
  - [Jellyfin 12.0 post](https://jellyfin.org/posts/jellyfin-release-12.0); [GitHub release v12.0](https://github.com/jellyfin/jellyfin/releases/tag/v12.0); [Help Net Security](https://www.helpnetsecurity.com/2026/09/08/jellyfin-12-0-security-fixes)
- Jellyfin API policy: "if an endpoint is not listed in the OpenAPI specification it should not be used… Deprecations will normally be marked for an entire major release cycle before removal." — [Jellyfin 12.0 post](https://jellyfin.org/posts/jellyfin-release-12.0)
- **Ferrofin (Rust):**
  - "A drop-in Rust implementation of the Jellyfin media server": GPL-3.0, created 2026-08-12, about 1,429 commits, 48 stars, last push 2026-10-30.
  - "All 412 operations in the Jellyfin API contract are wired to working handlers… No 501 stubs". About 360k lines of Rust, mostly AI-agent-written.
  - Reads and adopts Jellyfin 10.11.8–12.2 databases in place.
  - Clients named as working unchanged: jellyfin-web, Swiftfin, Findroid, Wolphin.
  - Emby-era headers (`X-Emby-Authorization`, `X-Emby-Token`) need `EnableLegacyAuthorization`, off by default as in Jellyfin 12.
  - [Ferrofin README](https://github.com/mangoleaf/ferrofin); [Ferrofin FEATURES.md](https://github.com/mangoleaf/ferrofin/blob/main/docs/FEATURES.md)
- Ferrofin verification depth: only "7 / 412 operations deep-verified against Jellyfin v12.2" (as of 2026-10-19). 1,833 transliterated upstream test cases vs 2,677 upstream. — [Ferrofin FEATURES.md](https://github.com/mangoleaf/ferrofin/blob/main/docs/FEATURES.md)
- Ferrofin parity gate: it replays 2,353 jellyfin-web requests. Of 2,307 compared, 1,242 are identical, 1,051 are accepted differences, 2 are filed issues and 12 are new. Request volume by area: `/Items` 490, `/Artists` 408, `/Shows` 325, `/Genres` 187, `/UserItems` 157, `/Sessions` 31, `/Videos` 23. — [Ferrofin README](https://github.com/mangoleaf/ferrofin)
- Ferrofin v1.4.0 vs Jellyfin 12.2.0 benchmark (same 32-core host, median of 3 runs, 2026-10-26):

  | Metric | Ferrofin | Jellyfin 12.2.0 |
  | --- | --- | --- |
  | Cold start | 275 ms | 3,333 ms (12.1×) |
  | Direct-play first byte | 0.64 ms | 4.12 ms (6.4×) |
  | HLS first segment | 184 ms | 431 ms (2.3×) |
  | PlaybackInfo | 1.99 ms | 7.82 ms |
  | `Sessions/Playing` | 0.83 ms | 8.55 ms |
  | Image | 0.66 ms | 4.66 ms |
  | Idle memory | 154 MiB | 393 MiB |
  | Peak memory | 206 MiB | 544 MiB |

  The benchmarked "screens" (home, detail, series, movies, playback) are made up of endpoint groups:
  - home: views, resume-video, nextup, latest-movies/shows/music
  - detail: item, similar, special-features
  - series: seasons, episodes
  - playback: playbackinfo, playing, stopped, intros, segments
  - images

  [Ferrofin README](https://github.com/mangoleaf/ferrofin)
- Client SDKs in Rust: `jellyfin-sdk` 0.1.0 (Jan 2026, MIT/Apache, about 6K SLoC, early stage, generated from the OpenAPI JSON) — [lib.rs](https://lib.rs/crates/jellyfin-sdk). `jellyfin-rs` (alpha, 3 commits) — [GitHub](https://github.com/KaiserBh/jellyfin-rs). An auto-generated Go client exists — [pkg.go.dev jellyfin-go](https://pkg.go.dev/github.com/Djancyp/jellyfin-go).

### Inferences
- A minimal "video-only" compatibility subset should be about 30–50 operations. This is inferred from Ferrofin's per-screen endpoint groups and general Jellyfin client behaviour; it is not counted from a source.
  - System: `GET /System/Info/Public`, `/System/Info`
  - Auth: `POST /Users/AuthenticateByName`, `GET /Users/Me`, QuickConnect
  - Browse: `/UserViews`, `/Items` (with filters, sort and fields), `/Items/{id}`, `/Items/Latest`, `/UserItems/Resume`, `/Shows/NextUp`, `/Shows/{id}/Seasons`, `/Shows/{id}/Episodes`, `/Items/{id}/Similar`
  - Playback: `POST /Items/{id}/PlaybackInfo` (with DeviceProfile), `/Videos/{id}/stream[.ext]`, `/Videos/{id}/master.m3u8` plus segments, subtitle stream routes
  - Images: `/Items/{id}/Images/{type}`
  - Sessions and progress: `POST /Sessions/Capabilities/Full`, `/Sessions/Playing`, `/Sessions/Playing/Progress`, `/Sessions/Playing/Stopped`
  - Watched state: `POST/DELETE /UserPlayedItems/{id}`
  - Media segments
  - The `/socket` WebSocket for UserDataChanged
- Infuse, Kodi add-ons and Jellyfin TV apps also call user and display preferences, `Branding` and `Localization` endpoints. Expect a long tail.
- Ferrofin's 12-week, mostly AI-written, all-412-routes approach shows that full compatibility is reachable. Its own numbers (7/412 deep-verified, about 46% of web requests answered identically) also show that bit-exact parity is the hard part.
- For our design, the key choice is GPL-3.0. Reusing Ferrofin code would make our server GPL. Implementing the API from the OpenAPI spec (the spec is a public interface) avoids that. That legal point is not verified here.
- Because 12.0 dropped `/emby` and legacy auth, a new server can target only the modern `Authorization: MediaBrowser …` header and current routes. Clients that still need legacy routes "have not seen an update in years".

### Gaps
- I did not fetch the OpenAPI JSON to count operations per tag myself. "412" comes from Ferrofin.
- I found no maintained Go reimplementation of the Jellyfin *server*, and none of the Plex API (Plex's client API is proprietary and account-bound).
- I did not confirm which exact endpoints Infuse, Kodi or Roku require. A capture via proxy against real Jellyfin is recommended.

## 4. Licensing/feasibility: writing native clients vs third-party; libmpv; Media3 FFmpeg

### Takeaway
Writing our own clients is feasible on mobile and desktop with libmpv/MPVKit or Media3+FFmpeg. Matching Infuse on tvOS or the TV-native apps on Roku/webOS/Tizen is costly. Reusing the Jellyfin client ecosystem through API compatibility is the fastest route. Own clients should come later, focused on multi-server/distributed UX.

### Cited Findings
- Swiftfin is MPL-2.0 (file-level copyleft) and already embeds MPV on iOS/tvOS. — [Swiftfin repo](https://github.com/jellyfin/swiftfin)
- Streamyfin is an Expo/TypeScript app with a native MPV module via MPVKit on all platforms. Chromecast subtitles are not done and there is no Roku or desktop build. — [Streamyfin README](https://github.com/streamyfin/streamyfin); [Hysen Labs](https://hysenlabs.com/projects/streamyfin-streamyfin)
- On Android, ExoPlayer (Apache-2.0) uses platform decoders, and its FFmpeg extension must be built by the app developer. — [Media3 supported formats](https://developer.android.com/media/media3/exoplayer/supported-formats). libVLC is LGPL/GPL and adds about 20–40 MB. — [LiveAPI blog (secondary)](https://liveapi.com/blog/exoplayer)
- MPV's software decoding on Apple "uses more power" for formats without hardware decode (VC-1, VP9). AVPlayer is listed as best for energy use and the only engine with AirPlay video handoff and tvOS framerate matching. — [Swiftfin players.md](https://github.com/jellyfin/Swiftfin/blob/main/Documentation/players.md)
- JellyCon is GPLv2. — [jellycon](https://github.com/jellyfin/jellycon)
- Infuse is closed source, with a freemium model (Pro $1.99/mo, $16.99/yr, $99.99 lifetime). — [Firecore](https://firecore.com/infuse)

### Inferences
- A hybrid native client is the best design for our own clients: a hardware/OS player (AVPlayer, ExoPlayer) for formats it handles, and mpv/FFmpeg as fallback. This keeps power use low and keeps OS features (AirPlay, framerate match) while avoiding server transcodes. Swiftfin #1853 users ask for exactly this ("default to AVPlayerLayer… VLCKit can then be a fallback"). — [Swiftfin #1853](https://github.com/jellyfin/Swiftfin/issues/1853)
- On Apple, the server can make AVPlayer viable for MKV by remuxing on the fly to fMP4/HLS, which is cheap. That still cannot fix DTS/TrueHD (needs an audio transcode) or PGS (burn-in or client render).

### Gaps
- I did not fetch mpv's or FFmpeg's license files in this session. mpv can be built LGPL (`-Dgpl=false`) and FFmpeg's DTS/TrueHD decoders are LGPL unless GPL components are enabled; both are well known but unverified here.
- App Store rules for LGPL dynamic linking on iOS/tvOS are unverified here.
- I found no 2026 source on Roku BrightScript or webOS/Tizen web-app effort estimates for a custom client.

## 5. Browser playback: MSE/ManagedMediaSource, WebCodecs, client-side remux in wasm/TS

### Takeaway
Browsers can avoid server remux for MKV by demuxing in the client: Mediabunny (pure TS, WebCodecs) reads MKV and writes fMP4 that MSE/ManagedMediaSource accepts. Codec limits remain: no DTS or TrueHD anywhere, no AC3 in Chrome/Firefox, HEVC hardware-only in Chromium. Client-side remux saves server CPU; it does not save audio or video transcodes.

### Cited Findings
- Safari 17.1 brought Managed Media Source (an evolution of MSE) to iPhone. Players must migrate MSE code to MMS. Apple still recommends native HLS. — [Radiant Media Player](https://www.radiantmediaplayer.com/blog/at-last-safari-17.1-now-brings-the-new-managed-media-source-api-to-iphone.html) (Nov 2023, older data)
- Mediabunny:
  - Pure TypeScript with zero dependencies, built on WebCodecs. Reads and writes MP4, MOV, WebM, MKV, HLS, MPEG-TS, Ogg, FLAC and more; "25+ video, audio, and subtitle codecs".
  - The conversion API covers transmuxing.
  - npm version 1.62.1.
  - [npm](https://www.npmjs.com/package/mediabunny); [GitHub](https://github.com/Vanilagy/mediabunny)
- Mediabunny performance (vendor benchmarks):
  - Metadata extraction: 862 ops/s, vs 233 (`@remotion/media-parser`), 80.6 (web-demuxer), 43.5 (mp4box.js), 1.83 (ffmpeg.wasm).
  - Packet iteration: 10,800 packets/s vs 2,390 (web-demuxer).
  - Bundle sizes: about 30 kB minified+gzipped for reading all formats, 69.6 kB for all features, vs 37.3 kB for mp4box.js.
  - [mediabunny.dev](https://mediabunny.dev)
- Jellyfin web avoids MKV in Firefox entirely and recommends forcing MKV→MP4 remux on webOS. — [Jellyfin Codec Support](https://jellyfin.org/docs/general/clients/codec-support)

### Inferences
- A web client should:
  1. Fetch the original file with HTTP range requests.
  2. Demux MKV with Mediabunny.
  3. Remux to fMP4 into MSE (MMS on iOS Safari).
  4. Render PGS/ASS on the client (e.g. a canvas renderer such as libass-wasm/JASSUB, not researched here).
  5. Request a server audio-only transcode (AAC/Opus) only when the browser lacks the codec.

  With this, the server serves byte ranges for almost every video track that Chrome/Edge/Safari can hardware-decode.
- This needs codec detection with `MediaSource.isTypeSupported` / `MediaCapabilities.decodingInfo`, and a DeviceProfile built from those answers rather than from static tables.

### Gaps
- I could not find the "playsvideo" project referenced in the brief.
- I found no caniuse/MDN 2026 numbers for WebCodecs or MMS coverage.
- I found no benchmarks of Mediabunny live remux latency for 4K remux-sized files (e.g. 60–80 Mbps), so time-to-first-frame vs server HLS remux is unmeasured. Ferrofin's 184 ms server-side HLS first segment is one reference point. — [Ferrofin README](https://github.com/mangoleaf/ferrofin)
