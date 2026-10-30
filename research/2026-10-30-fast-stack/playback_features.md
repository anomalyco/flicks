# User-facing video & TV playback features for a modern home media server (state as of Oct 2026)

Scope: user-facing playback/TV features only (transport, transcoding, scanning, sync and codecs are covered in sibling notes). Research method note: the Parallel search MCP was rate-limited, so research used the built-in web search plus direct fetches of primary pages (Jellyfin release posts and docs, Plex support articles and blog, Firecore/Infuse blog, Channels site, Apple/Android developer pages) and news coverage (XDA, How-To Geek, Android Authority, MacRumors, 9to5Mac). Items not verified in this session are explicitly labelled "background knowledge, unverified" in Inferences, or listed under Gaps.

**Big 2026 context the report writer should know:**
- Jellyfin shipped **12.0 on 2026-09-07** (they dropped the "10." prefix; what would have been 10.12 is 12.0). It removed the legacy `/emby/` and `/mediabrowser/` routes and disables deprecated auth by default, which matters for any Jellyfin-API-compatible server — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin's founding leadership left in **July 2026** (project leader Joshua Boniface, Anthony Lavado, co-founder Andrew Rabert), citing burnout — [Linuxiac](https://linuxiac.com/jellyfin-loses-project-leader-and-core-team-member-in-major-shake-up/); [XDA](https://www.xda-developers.com/jellyfin-core-team-left-still-not-going-back-plex/); [Cybernews](https://cybernews.com/tech/jellyfin-founders-step-down-future-uncertain/)
- Plex raised the price of a new **Lifetime Plex Pass from $249.99 to $749.99 on 2026-07-01** — [Plex blog](https://www.plex.tv/blog/new-lifetime-plex-pass-pricing/); [9to5Mac](https://9to5mac.com/2026/05/19/plex-increasing-lifetime-plex-pass-cost-to-whopping-750/)
- In **Aug 2026**, after more than a year of complaints, Plex brought back the TV-app sidebar it removed in its redesign — [How-To Geek](https://www.howtogeek.com/plex-brings-back-sidebar-navigation-after-users-refuse-to-let-it-go/); [XDA](https://www.xda-developers.com/after-months-backlash-plex-rolling-back-controversial-big-screen-app-redesign/)

---

## Core viewing: resume, Continue Watching/Next Up, autoplay, skip intro/recap/credits, chapters, trickplay, audio/subtitle tracks and preferences, subtitle search/sync, audio delay

### Takeaway
Skipping segments (intro, recap, credits, preview, commercial) is now table stakes in every serious client, and the industry has settled on **typed time-range markers stored by the server and acted on by the client**. Jellyfin's Media Segments API (10.10+) is the de facto interoperable format. Markers come from three sources: local audio/chapter analysis, a cloud hash-keyed cache (Plex), or community timestamp databases (IntroDB/TheIntroDB, used by Infuse 8.4). Trickplay thumbnails, chapter names on the seek bar, a "still watching?" prompt, frame stepping, and marking an item played at the credits marker are the 2024–2026 polish items.

### Cited Findings
**Skip markers / media segments**
- Jellyfin Media Segments (introduced in 10.10) are typed time spans on a media file. Unlike chapters they carry a type: **Intro, Outro, Recap, Preview, Commercial**. "Jellyfin can store this information and provide it to clients. Clients can then decide what they want to do". The action for each segment type is set per client in its playback settings — [Jellyfin docs: Media segments](https://jellyfin.org/docs/general/server/metadata/media-segments/)
- Segments are created by provider plugins. An official "Chapter Segments Provider" builds them from chapter names. A scheduled "Media segment scan" task fills them in — [Jellyfin docs](https://jellyfin.org/docs/general/server/metadata/media-segments/)
- At 10.10.0 (2024-10-26), "we only provide the general structure… you will still require a plugin to create them". The web client fully supported skipping; "other client support is pending" — [Jellyfin 10.10.0](https://jellyfin.org/posts/jellyfin-release-10.10.0/)
- Skip intro is not a native Jellyfin detection feature. The community **Intro Skipper** plugin analyzes episode audio to find recurring intros and credits. Since 10.10 it acts as a media-segment provider — [intro-skipper GitHub](https://github.com/intro-skipper/intro-skipper); [r/selfhosted](https://www.reddit.com/r/selfhosted/comments/1dbiw90/skip_intro_support_in_jellyfin/)
- Infuse added Skip Intro, Preview and Credits for Jellyfin 10.10+ in Infuse 8.0.4 (Nov 2024) — [Infuse on X](https://x.com/infuse/status/1859309381118275846)
- **Infuse 8.4 (2026-03-24)** added skipping of intros, credits and recaps for *all* share types (Plex/Emby/Jellyfin/SMB/cloud). It uses two community timestamp databases, **IntroDB.app and TheIntroDB.org** ("similar to TMDB"), with auto-skip or a manual button — [Firecore blog](https://firecore.com/blog/infuse-84-extras-intros-and-favorites)
- Pitfall: community timestamps can be keyed to a different *copy* of an episode. A user reported TheIntroDB marking an intro at 0:00–0:45 from a release without the recap, which conflicted with Jellyfin Intro Skipper markers on their own file — [Firecore community](https://community.firecore.com/t/infuse-using-both-the-intro-database-and-jellyfin-intro-skipper-markers-when-timestamps-differ/60000)
- An Emby plugin pulls TheIntroDB segments and shows them through Emby's native Skip Intro button, which requires Emby Premiere — [TheIntroDB emby-plugin](https://github.com/TheIntroDB/emby-plugin)
- **Plex credits detection** (PMS 1.31.0+) is paywalled twice: the server admin needs Plex Pass to analyze, and the account playing needs Plex Pass (or to be in a Plex Home whose admin has it). It is supported on Fire TV, Android, Android TV, Apple TV, iOS, desktop, PlayStation, HTPC, Web, Roku, smart TVs and Xbox — [Plex Support: Credits Detection](https://support.plex.tv/articles/credits-detection/)
- Plex skips to mid-credits and post-credits scenes when it detects them. On the final credits marker, some apps shrink the video into the post-play screen — [Plex Support](https://support.plex.tv/articles/credits-detection/); [Plex blog](https://www.plex.tv/blog/let-the-next-episode-roll/)
- Plex shares **credits markers through a cloud cache keyed by file hash**: "both, try online first" is the default, alternatives are online only or local only, and no title information is sent. Intro markers are *not* shared to the cloud — [Plex Support](https://support.plex.tv/articles/credits-detection/)
- Plex can mark an item played at the threshold percentage (90% by default), at the first or final credits marker, or at whichever comes first between the threshold and the first credits marker (the default) — [Plex Support](https://support.plex.tv/articles/credits-detection/)
- **Emby Intro Skip** requires Emby Server 4.7+ and Emby Premiere. Detection runs per TV season, and each client can be set to ignore it — [Emby docs](https://emby.media/support/articles/Intro-Skip.html)

**Trickplay / scrubbing / chapters**
- Jellyfin **10.9** added native trickplay (seek thumbnails), replacing the Jellyscrub plugin. Third-party clients had to adopt the new API — [Delfin issue #101](https://codeberg.org/avery42/delfin/issues/101); [Findroid issue #801](https://github.com/jarnedemeulemeester/findroid/issues/801)
- Jellyfin 10.10 added optional keyframe-only trickplay extraction, about 100x faster ("no more multi-day Trickplay jobs") at some cost in frame accuracy — [Jellyfin 10.10.0](https://jellyfin.org/posts/jellyfin-release-10.10.0/)
- Jellyfin 12.0 web client: chapter names appear in the seek-bar preview bubble, `,` and `.` step frame by frame, there is a **"still watching" prompt**, and remotes' rewind and fast-forward buttons work. Trickplay reuses existing files on scan and handles interlaced video and bad timestamps — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)

**Continue Watching / Next Up / versions**
- Jellyfin 12.0 made Continue Watching, Next Up and rewatching faster as part of database tuning. It also added **multiple versions for episodes** (broadcast vs extended cut, 1080p vs 4K), with "resume data that follows the version you were actually watching" — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Channels' DVR markets "Up Next" (suggests what to watch next from history), "Auto Play" of the next episode and "Shuffle" — [Channels Live TV & DVR](https://getchannels.com/live-tv/)

**Audio/subtitle tracks, preferences, subtitle search & sync**
- Plex has **On-Demand Subtitle Search** from the pre-play screen or during playback for movie and TV libraries — [Plex Support](https://support.plex.tv/articles/subtitle-search/)
- Plex has **Auto-Sync Subtitles**; analyzing content for it requires Plex Pass on the server admin account (from a search snippet of Plex's support article) — [Plex Support: Auto-Sync Subtitles (search result)](https://support.plex.tv/articles/subtitle-search/)
- Jellyfin 12.0 moved subtitle settings from server-wide to per library and added an option to prefer a title's **original language for audio**. Clients can now request responses in a specific language, which helps multi-language households — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin 12.0 subtitle pipeline: written through SubtitleEdit, which avoids the old SSA→ASS styling loss; VobSub support; image subtitles rendered client-side while remuxing. Jellyfin 10.10 added client-side PGS rendering in web — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/); [Jellyfin 10.10.0](https://jellyfin.org/posts/jellyfin-release-10.10.0/)
- Jellyfin user settings include "Remember audio selections", "Remember subtitle selections" and "Automatically play the next episode". This comes from a low-quality third-party page quoting Jellyfin's settings; verify against Jellyfin's UI — [scene-ftp.com (weak source)](https://scene-ftp.com/)

### Inferences
- **The server owns the segment data and the client owns the action.** This is the right split. For a Rust server, implement Jellyfin's `/MediaSegments` shape so Jellyfin clients (web, Android TV, Swiftfin, Streamyfin, Infuse) light up skip buttons for free. Fill it from (a) chapter names, (b) local audio fingerprinting (Intro Skipper's approach: chromaprint across episodes of a season), and (c) optional lookups in IntroDB or TheIntroDB keyed by TMDB/TVDB id plus episode. Store a confidence and source with each segment, because community data can mismatch a given file.
- **A distributed design gets a free win:** segments computed on one home server can be shared to peers keyed by a content hash, like Plex's hash-keyed cloud cache but peer-to-peer. No central account server is needed.
- Use credits markers to decide when an item counts as "watched", as Plex does. This fixes the common complaint that items stay in Continue Watching while the credits roll. It needs server logic plus segment data.
- Background knowledge, unverified in this session: Netflix and the Apple TV app set the user benchmark of a skip-intro/recap button, a next-episode card with countdown during credits, and a "still watching?" prompt. Plex has offered "Skip Intro" for years as a Plex Pass feature, analyzed per season.
- Audio delay and subtitle offset adjustment are **client-only** features (Infuse, Kodi and VLC-based players have them). Subtitle search and download (OpenSubtitles) can be server-side (Plex, Jellyfin's OpenSubtitles plugin) or client-side (Infuse). Server-side is better for a multi-client home because the file is fetched once and works everywhere.
- Per-show language memory needs **server-side per-user, per-series storage** to roam across devices. Netflix-style "remember my last audio/subtitle choice for this show" is the expected behaviour. Jellyfin exposes "remember selections" (weakly sourced). Whether it is stored per series server-side is unverified.

### Gaps
- No primary source fetched for Plex's Skip Intro requirements, for Plex or Jellyfin per-show audio and subtitle memory semantics, or for forced-subtitle handling in each product.
- No source fetched for the Netflix or Apple TV app autoplay countdown length or behaviour. This is background knowledge only.
- No 2024–2026 source found for audio-delay or subtitle-offset support per client. Kodi, Infuse and VLC-based players are believed to have it (unverified).

---

## TV / 10-foot experience: frame-rate & dynamic-range matching, HDR/DV passthrough, audio passthrough, loudness/night mode, remote navigation, profiles/PINs, screensaver, launch speed

### Takeaway
Frame-rate and dynamic-range matching are OS features that **each app must explicitly opt into**: AVDisplayCriteria/AVDisplayManager on tvOS and `setFrameRate()` on Android. Clients that skip this get judder or SDR output, as Streamyfin and Swiftfin bug reports show. Infuse remains the reference Apple TV client for HDR/Dolby Vision and lossless audio. For 10-foot UI, the 2025–2026 Plex saga shows that remote-navigation muscle memory (a left-hand sidebar, few clicks to reach your own libraries) matters more than visual redesign.

### Cited Findings
- Apple TV: Settings > Video and Audio > **Match Content** has Match Dynamic Range and Match Frame Rate, which switch the output to the content's native frame rate and range — [Apple Support 102277](https://support.apple.com/en-us/102277)
- Starting with tvOS 11.2, Apple TV 4K can switch display modes to match native frame rate and dynamic range. Apps take part through AVDisplayManager and display criteria (2017–18 tech talk, *older* but still the API) — [Apple Tech Talk: Introduction to AVDisplayManager](https://developer.apple.com/videos/play/tech-talks/503/)
- An example of what happens when a client doesn't implement it: Streamyfin's Apple TV bug "Display refresh rate is never switched to match the content", even with Match Frame Rate enabled — [Streamyfin #2261](https://github.com/streamyfin/streamyfin/issues/2261)
- An older Swiftfin bug showed Dolby Vision MKVs playing as SDR where Infuse displayed DV — [Swiftfin #693](https://github.com/jellyfin/Swiftfin/issues/693)
- Android: apps call `setFrameRate()` on the surface. Android's guidance says it is cheap but should not be called every frame — [Android Developers: Frame rate](https://developer.android.com/media/optimize/performance/frame-rate)
- *(Older, 2022)* Android 12's "match content frame rate" was "a work in progress" and did not work with the major streaming apps tested, unlike Apple TV 4K — [FlatpanelsHD](https://www.flatpanelshd.com/news.php?subaction=showfull&id=1666253858)
- Infuse direct-plays 4K HDR, single-layer Dolby Vision and Dolby Atmos from Jellyfin — [Jellyfin client spotlight: Infuse](https://jellyfin.org/posts/client-infuse/)
- The Infuse App Store listing claims HDR, HDR10+, Dolby Vision, Dolby and DTS audio — [App Store: Infuse](https://apps.apple.com/no/app/infuse/id1136220934)
- Jellyfin 12.0 client device support: Dolby Vision in MKV on webOS 25+, AV1 direct streaming on TV clients, anamorphic direct play on Tizen, iOS background playback, and a fix for audio normalization changing pitch and speed on iOS. There is also a device-profile flag for Android TV boxes that can't handle rotated video — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin 12.0 TV/remote polish: game-controller navigation fixes, rewind and fast-forward remote buttons, screensaver suppressed while viewing photos or reading, a screensaver time setting, and a "still watching" prompt. TV devices still use the legacy TV layout — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin 12.0 "apply[s] parental controls in more places" (a security fix) — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Plex TV redesign: users complained for a year that the new **top-bar navigation was too slow and unintuitive**. It broke the muscle memory of pressing left to reach a sidebar, and reaching a personal library took extra menu hops. In Aug 2026 Plex restored the sidebar, made favourite libraries top-level sources, made Live TV/DVR sources reorderable, and fixed performance and focus bugs — [How-To Geek](https://www.howtogeek.com/plex-brings-back-sidebar-navigation-after-users-refuse-to-let-it-go/); [XDA](https://www.xda-developers.com/after-months-backlash-plex-rolling-back-controversial-big-screen-app-redesign/)
- Channels' live TV app advertises "Quick Tune: change channels in less than 1 second", full VoiceOver support, closed captions and alternate audio tracks, and 5.1 surround — [Channels Live TV](https://getchannels.com/live-tv/)

### Inferences
- **Client-only, but the server must supply accurate metadata.** Frame-rate and range matching need exact frame rate (including 23.976 vs 24), HDR format (HDR10, HDR10+, HLG, DV profile and level, DV compatibility id) and audio codec/profile (TrueHD+Atmos, DTS-HD MA/DTS:X) in the item's MediaStreams. The client needs these *before* starting playback so it can switch display mode first. This is a server responsibility in a Jellyfin-compatible API.
- Audio passthrough (Atmos/TrueHD/DTS:X) is a client and device capability. The server's job is not to transcode it away, so codec profiles must be accurate.
- Background knowledge, unverified in this session: night mode and dynamic range compression ("Reduce Loud Sounds" on tvOS, Netflix/Apple dialogue boost), and Plex's per-user PIN-protected Plex Home profiles with ratings restrictions. Kodi offers per-file audio offset, refresh-rate switching ("Adjust display refresh rate") and passthrough on capable hardware. Profiles with PINs and per-profile parental ratings are a server-side permission model and should be built in from day one.
- Fast app launch and fast home screens depend mostly on server API latency for home rows (Continue Watching, Next Up, Latest). Jellyfin 12.0 explicitly optimized these queries ([Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)). A Rust server can win here with precomputed or cached home rows.

### Gaps
- No primary 2024–2026 source on audio loudness normalization or night mode across Plex, Jellyfin and Infuse. Jellyfin has ReplayGain for music (12.0 reads album gain) and an "audio normalization" setting on iOS, per [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/). Video dialogue boost support was not found.
- No current source on Plex Home PIN and parental-rating behaviour, or on Jellyfin's per-user max parental rating. Both are background knowledge.
- No 2024–2026 test of Android TV/Google TV frame-rate matching quality. The latest source is 2022.

---

## Multi-device: casting, remote control, watch together/SyncPlay, downloads/offline with progress sync, handoff

### Takeaway
Group watching is a proven, emotional feature. Plex **removed Watch Together** from its new apps in Feb 2025 (it is left only in the web app), drew forum outrage, and pushed users to Jellyfin's built-in **SyncPlay**. Jellyfin SyncPlay is free and built in, but client coverage and robustness are uneven. A third-party "SyncPlay v2" plugin exists specifically to recover from stalls and dropped connections. Channels and Plex treat offline downloads as standard. Casting and remote control were not well covered by 2024–2026 sources in this session.

### Cited Findings
- Plex: "Watch Together will not be available in the new Plex experience for most devices". It continues in the web app "for the foreseeable future", and Plex may offer "similar functionality again in the future, using new tooling" (Feb 26, 2025) — [Android Authority](https://www.androidauthority.com/plex-watch-together-removed-3529939/); [PCWorld](https://www.pcworld.com/article/2619590/plex-is-dropping-a-popular-feature-from-its-new-streaming-apps.html); [Plex Support: Watch Together](https://support.plex.tv/articles/watch-together/)
- User reaction: "Please do NOT remove Watch Together" forum thread (May 2025), and "restrict it to Plex Pass paid users, but don't eliminate it entirely" — [Plex Forum](https://forums.plex.tv/t/please-do-not-remove-watch-together/906929); [Plex Forum suggestion](https://forums.plex.tv/t/add-watch-together-to-new-plex-experience/906941)
- Android Authority published a guide to Jellyfin SyncPlay as the replacement. SyncPlay "allows anyone connected to your server to start or join groups to synchronize playback" — [Android Authority](https://www.androidauthority.com/jellyfin-syncplay-explained-3530437/)
- A third-party "drop-in replacement for Jellyfin SyncPlay" server plugin (protocol v2, still compatible with v1 clients) says it "recovers from stalled loads, brief buffering, and dropped connections". This implies those are weak points of the stock implementation — [kontell/jellyfin-plugin-syncplayv2](https://github.com/kontell/jellyfin-plugin-syncplayv2)
- A request to add SyncPlay to Jellyfin's Android TV client (#538) has 60+ 👍, so coverage has been incomplete on at least one first-party TV client (current status not verified) — [jellyfin-androidtv #538](https://github.com/jellyfin/jellyfin-androidtv/issues/538)
- Channels lists Offline Downloads, Stream While Away, and Channel Collections synced across all devices or applied per device ("playroom, garage, kitchen TV") — [Channels Live TV](https://getchannels.com/live-tv/)
- Channels Multiview: watch four things at once on Apple TV and iPad, with multiple layouts and instant switching — [Channels Live TV](https://getchannels.com/live-tv/)

### Inferences
- **SyncPlay is server-coordinated:** the server keeps a group clock and state machine, and clients follow commands. A distributed server design must decide which node owns a group (the server hosting the media or the one the host is on). Implementing Jellyfin's SyncPlay WebSocket protocol gives compatibility with existing clients. Robust drift correction and buffering recovery is a differentiator users ask for.
- Background knowledge, unverified in this session: Plex Companion lets one Plex app control another, and Jellyfin has "remote control" sessions via its Sessions API and WebSocket. Both are **server-relayed**, so a Jellyfin-compatible server must implement session listing and `/Sessions/{id}/Playing` and similar commands. Google Cast needs a receiver app (Plex and Jellyfin each have one). AirPlay is client/OS only.
- Downloads with progress sync back need the server to accept **offline-recorded playstate with timestamps** and resolve conflicts (last-write-wins by timestamp, or max position). See the sibling state-sync note.
- Handoff, where you start on the phone and continue on the TV, falls out of fast, consistent resume position sync across servers and clients. No product markets explicit Apple-style "Handoff" for home media in sources found.

### Gaps
- No 2024–2026 primary sources fetched for Plex Companion, the Jellyfin remote-control UX, Chromecast/AirPlay support per client, or Plex/Jellyfin download feature details (e.g. Plex Downloads requiring Plex Pass, Jellyfin mobile downloads). These are background knowledge only.
- No user-sentiment data on casting quality in 2025–2026.

---

## Library niceties: collections, playlists, watchlist, recommendations, recently added, multiple versions/editions, extras/trailers/theme music, cast & crew, ratings

### Takeaway
Expected baseline: collections and box sets, playlists, recently added, cast and crew pages, extras, and multiple versions. The 2026 frontier is **versions that work for episodes with version-aware resume** (Jellyfin 12.0), **pluggable recommendation sources** (Jellyfin 12.0), and **extras as first-class content in third-party clients** (Infuse 8.4). Plex's push of streaming "Discover" and Universal Watchlist content into personal-media users' home screens was widely disliked.

### Cited Findings
- Jellyfin 12.0: playlists and collections are stored as one row per item, so large playlists page and edit fast. Collections and playlists tabs appear on all libraries, collections show on item detail pages, and collections and playlists can be filtered by library — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin 12.0: the source for "more like this" and suggestion rows is chosen **per library** (pluggable providers; ListenBrainz ships for music), and plugins can add search results — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin 12.0: alternate versions now work for episodes, with resume following the version watched. Auto-grouped versions are rebuilt by a required rescan — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin 12.0 also added: Play All and Shuffle on series libraries, an improved Upcoming view, crew with multiple roles merged into one card, TV show creators on detail pages, folders markable as played, and audio and subtitle language filters — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Infuse 8.4 shows extras (deleted scenes, featurettes, shorts, trailers) from Emby, Jellyfin and Plex on the title's details page — [Firecore blog](https://firecore.com/blog/infuse-84-extras-intros-and-favorites)
- Infuse 8.5 (2026-07-30): long-press Play to choose a server-transcoded version, or pick from multiple versions with a "Version" button. Plex, Emby and Jellyfin are supported — [Firecore blog](https://firecore.com/blog/infuse-85-smarter-streaming)
- Plex Discover and Universal Watchlist (launched April 2022, *older*) got a "mixed response". Users ask how to turn Discover off — [ktz blog timeline](https://blog.ktz.me/the-enshittification-of-plex/)
- Users in 2026 still complain about the new Plex app. A suggested fix is disabling the Discover source and "more ways to watch" under Online Media Sources — [r/PleX "The new Plex version sucks hard!"](https://www.reddit.com/r/PleX/comments/1utbtal/the_new_plex_version_sucks_hard/)
- The Verge (Nov 2024) described the redesign's goal as navigation that "more clearly delineates" between server media and Plex's own streaming content — [The Verge](https://www.theverge.com/2024/11/22/24303282/plex-app-redesign-sonos-debacle)
- Channels Media Library: Personal Sections, Library Collections, Playlists, Virtual Channels, and Kids, Sports and News sections — [Channels Live TV](https://getchannels.com/live-tv/)

### Inferences
- Must-have: Continue Watching, Next Up (the next unwatched episode after the last watched one), Recently Added grouped by show, collections, playlists, a cast-and-crew filmography from your own library, and versions/editions with a per-user default (e.g. prefer 4K HDR on TV, 1080p on mobile). Versions are a server-side grouping and selection concern. Per-device preference can be client-side.
- Delight: virtual channels / "TV mode" (Channels), shuffle a show, theme music and backdrop videos on detail pages (Plex, Emby and Jellyfin offer theme songs; background knowledge), and "because you watched" rows. A **local, private** recommender (co-watch and metadata similarity) fits the no-central-server ethos and is a differentiator against Plex's ad-driven Discover.
- Avoid: mixing third-party streaming catalogue content into personal-media home screens by default. It is the most persistent Plex complaint.

### Gaps
- No primary source fetched for Plex editions (the `{edition-…}` naming), Plex theme music, or Emby collections and recommendations.
- No data on how much users use recommendations in personal media servers.

---

## Live TV & DVR: HDHomeRun/IPTV (M3U/XMLTV), guide data, recording, commercial detection (comskip)

### Takeaway
**Channels DVR ($8/mo or $80/yr)** is the enthusiast benchmark for live TV. It combines HDHomeRun, TV Everywhere and custom M3U sources into one guide, quick tune under 1 second, automatic commercial indexing with auto-skip, Series/Team Passes, multiview, and flat-file recordings. Plex and Jellyfin both offer HDHomeRun/M3U/XMLTV DVR. Jellyfin 12.0 improved XMLTV and HDHomeRun handling. Commercial detection is the most-loved and most bug-prone DVR feature.

### Cited Findings
- Channels sources: HDHomeRun (over-the-air or CableCARD), **TV Everywhere** (sign in with your cable or streaming provider), and **Custom Channels** (any live internet stream, security cameras, SAT>IP) — [Channels Live TV](https://getchannels.com/live-tv/); [Channels TVE](https://getchannels.com/tv-everywhere/)
- Channels price: $8/month or $80/year, first 30 days free — [Channels subscription docs](https://getchannels.com/docs/getting-started/quick-start-guide/subscription/); [Channels Live TV](https://getchannels.com/live-tv/)
- Channels DVR features: Series Pass, Team Pass, watching while recording, Up Next, Auto Play, Shuffle, Kids section, Auto Prune, **flat files** (recordings are plain video files on disk), tuner sharing (one tuner for the same channel on every device), hardware transcoding, multiple prioritized HDHomeRuns, "Sports Seek" (custom seek times for sports), channel flipping, Multiview (4-up), Channel Collections, and "On Later" (browse upcoming movies, sports, new shows and seasons) — [Channels Live TV](https://getchannels.com/live-tv/)
- "Channels indexes commercials after each recording is completed", and recordings then skip commercials automatically — [Channels Support: Commercial Skipping](https://getchannels.com/docs/apps/usage/commercial-skipping/); [Channels Live TV](https://getchannels.com/live-tv/)
- Complaints: a DVR beta broke commercial detection on some sources, e.g. CBS shows — [Channels Community](https://community.getchannels.com/t/commercial-detection-bug/43498). A user found commercial detection kept running after they disabled it in Passes for one show — [r/getchannels](https://www.reddit.com/r/getchannels/comments/1ov5gxj/channels_dvr_newbie_disable_skip_commercials_by/)
- Infuse users have long asked for EDL (comskip edit decision list) support to skip commercials in DVR recordings: "would be a killer feature" — [Firecore community](https://community.firecore.com/t/edl-support-for-comskip-plex-channels-dvr-recordings/16823)
- Jellyfin 12.0 Live TV: imports XMLTV background images and episode thumbnails, skips unchanged programmes on guide refresh ("considerably cheaper"), honours Schedules Direct error codes to avoid lockouts, **direct-plays HDHomeRun** on supporting clients, defaults M3U tuners to remux, and stops returning unreachable "server-local" stream URLs to clients — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)
- Jellyfin media segments include a `Commercial` type, so DVR commercial markers can travel over the same API as intros — [Jellyfin docs](https://jellyfin.org/docs/general/server/metadata/media-segments/)
- Reddit debate: Channels is "designed specifically for tv consumption and recording", versus doubts about paying for it — [r/getchannels](https://www.reddit.com/r/getchannels/comments/1i0del7/channels_dvr_better_than_plex/)

### Inferences
- **Live TV is should-have, not must-have**, for a lightweight server. Cord-cutters care a lot, and Channels shows they pay for it, but most self-hosters mainly want on-demand. A pragmatic v1: HDHomeRun discovery plus M3U/XMLTV ingest, a grid guide, Series Pass recording to flat files, and comskip-style detection emitted as `Commercial` media segments. Jellyfin clients will then show skip buttons with no special DVR client logic.
- Tuner sharing and quick tune are server-side: one tuner stream is fanned out to many viewers, and the HDHomeRun HTTP stream is kept warm. Multiview is client-side.
- TV Everywhere is legally and technically hairy (provider auth, DRM-free sources only). It is likely out of scope.

### Gaps
- No usage or popularity numbers found for Plex DVR, Jellyfin Live TV or Channels (no subscriber counts).
- No 2024–2026 primary source fetched on Plex DVR commercial skipping (it existed historically via comskip, which can remove or mark commercials) or on Plex's free live TV channels.

---

## What generates the most user love and complaints (2024–2026)

### Takeaway
Users react most strongly to **removals and paywalls**, not missing new features. Examples: Plex's remote-playback paywall (Apr 2025), Watch Together removal (Feb 2025), the TV redesign's loss of sidebar navigation (2025 to Aug 2026 rollback), and the $749.99 lifetime pass (Jul 2026). Features users love: skip intro and credits, SyncPlay/Watch Together, Infuse's "it just plays everything with DV/Atmos", and Channels' commercial skipping. Jellyfin's sore spots: skip intro and trickplay arrived late and through plugins, uneven client parity, and 2026 governance uncertainty.

### Cited Findings
- Since **2025-04-29**, remote playback of personal video from a Plex server requires the viewer or server owner to have Plex Pass, or the viewer to buy a **Remote Watch Pass ($1.99/mo or $19.99/yr)** — [Plex Support](https://support.plex.tv/articles/requirements-for-remote-playback-of-personal-media/); [Plex blog](https://www.plex.tv/blog/important-2025-plex-updates/); [Bytesized guide](https://bytesized-hosting.com/guides/plex-2025-slash-2026-changes-what-seedbox-users-need-to-know)
- Plex Pass went to $6.99/mo, $69.99/yr and $249.99 lifetime on Apr 29, 2025 — [MacRumors](https://www.macrumors.com/2025/03/19/plex-price-increase/)
- Lifetime rose to **$749.99 from Jul 1, 2026** — [Plex blog](https://www.plex.tv/blog/new-lifetime-plex-pass-pricing/); [MacRumors 2026](https://www.macrumors.com/2026/05/19/lifetime-plex-pass-price-increase/). An XDA commenter: "The $750 lifetime cost is what pushed me to jump ship and board the jellyfin boat" (anecdotal) — [XDA](https://www.xda-developers.com/after-months-backlash-plex-rolling-back-controversial-big-screen-app-redesign/)
- The TV redesign hit Roku in Sept (2025), drew heat "almost immediately", and a fall 2025 fix "wasn't enough". In Aug 2026 Plex brought back the sidebar — [How-To Geek](https://www.howtogeek.com/plex-brings-back-sidebar-navigation-after-users-refuse-to-let-it-go/)
- Plex forum thread title: "Releasing the new app was a crazy decision" (22+ pages) — [Plex Forum](https://forums.plex.tv/t/releasing-the-new-app-was-a-crazy-decision/910913?page=22)
- Paywalled polish: Plex credits skipping needs Plex Pass on both admin and viewer — [Plex Support](https://support.plex.tv/articles/credits-detection/). Emby Intro Skip needs Emby Premiere — [Emby docs](https://emby.media/support/articles/Intro-Skip.html)
- Jellyfin: skip intro needed a plugin. 10.10 (Oct 2024) only added the segment storage API, and client support was "pending" — [Jellyfin 10.10.0](https://jellyfin.org/posts/jellyfin-release-10.10.0/). Infuse users had a long-running "Support skip intro for Jellyfin" request, closed Dec 2024 — [Firecore community](https://community.firecore.com/t/support-skip-intro-for-jellyfin/37841?page=3)
- Jellyfin governance: founders and project leader stepped down in July 2026 — [Linuxiac](https://linuxiac.com/jellyfin-loses-project-leader-and-core-team-member-in-major-shake-up/); [Cybernews](https://cybernews.com/tech/jellyfin-founders-step-down-future-uncertain/)
- Jellyfin 12.0 breaking changes for clients: legacy `/emby/` and `/mediabrowser/` routes removed, deprecated authorization disabled by default, and GetItems semantics changed — [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)

### Inferences
- Product lesson for a new server: **never paywall or remove basic playback niceties** (remote play, skip markers, watch together, downloads), and keep TV navigation stable and remote-efficient. These are exactly the levers that drove Plex users to Jellyfin in 2025–2026.
- Jellyfin-API compatibility is a moving target. Target the 12.0 OpenAPI spec (with modern auth headers), not legacy `/emby/` routes. The leadership change adds some risk to the API's future direction. Watch for a fork or rename.

### Gaps
- No quantitative sentiment data (survey or poll counts) was found. The evidence is forum, Reddit and press coverage.
- r/appletv sentiment on Match Content was not researched beyond Apple's docs.

---

## Prioritized catalog (must / should / delight) with server-vs-client split and who has it

### Takeaway
A Jellyfin-compatible Rust server can deliver most of the "nice stuff" by owning **accurate stream metadata, per-user playstate and preferences, typed media segments, trickplay tiles, versions, SyncPlay and session relay, and Live TV/DVR**. Clients own presentation: display-mode switching, passthrough, subtitle rendering and offset, and audio delay.

### Cited Findings
(The table synthesizes the findings above. Each "has it" cell cites a source from earlier sections where one was found; "bk" = background knowledge, unverified.)

| Priority | Feature | Server vs client | Plex | Jellyfin | Emby | Infuse | Channels | Notes / sources |
|---|---|---|---|---|---|---|---|---|
| Must | Resume / Continue Watching / Next Up | Server (playstate) + client UI | yes (bk) | yes, faster in 12.0 | yes (bk) | yes (bk) | Up Next ([Channels](https://getchannels.com/live-tv/)) | [Jellyfin 12.0](https://jellyfin.org/posts/jellyfin-release-12.0/) |
| Must | Autoplay next episode + countdown | Client (server supplies Next Up) | yes (bk) | yes ("Automatically play the next episode", weak source) | bk | bk | Auto Play | [Channels](https://getchannels.com/live-tv/) |
| Must | Skip intro/recap/credits/preview | Server stores typed segments; client acts | Pass-only ([Plex](https://support.plex.tv/articles/credits-detection/)) | Media Segments 10.10+, plugins create ([docs](https://jellyfin.org/docs/general/server/metadata/media-segments/)) | Premiere-only ([Emby](https://emby.media/support/articles/Intro-Skip.html)) | community DBs, all sources ([8.4](https://firecore.com/blog/infuse-84-extras-intros-and-favorites)) | commercials only | Add a peer-shared, hash-keyed segment cache |
| Must | Trickplay thumbnails + chapter names | Server generates tiles; client renders | yes (bk) | native since 10.9; fast keyframe mode 10.10 | bk | bk | — | [10.10](https://jellyfin.org/posts/jellyfin-release-10.10.0/), [12.0](https://jellyfin.org/posts/jellyfin-release-12.0/) |
| Must | Audio/subtitle selection, remembered per user/show | Server stores prefs; client applies | bk | remember selections (weak source); original-language audio option 12.0 | bk | bk | CC + alt audio | — |
| Must | Accurate HDR/DV/frame-rate/audio metadata for match-content and passthrough | Server metadata; client switching | — | — | — | reference client ([Jellyfin spotlight](https://jellyfin.org/posts/client-infuse/)) | — | [Apple](https://support.apple.com/en-us/102277), [Android](https://developer.android.com/media/optimize/performance/frame-rate) |
| Must | Remote-friendly TV navigation, fast home rows | Client + server query latency | regressed then restored ([HTG](https://www.howtogeek.com/plex-brings-back-sidebar-navigation-after-users-refuse-to-let-it-go/)) | TV layout still legacy | — | — | quick tune <1 s | — |
| Must | Profiles, parental ratings, PINs | Server | Plex Home (bk) | parental controls ([12.0](https://jellyfin.org/posts/jellyfin-release-12.0/)) | bk | — | Kids section | — |
| Should | Subtitle search/download + auto-sync | Server (fetch once) or client | On-demand search ([Plex](https://support.plex.tv/articles/subtitle-search/)); auto-sync Pass-only | OpenSubtitles plugin (bk) | bk | bk | — | — |
| Should | Multiple versions/editions incl. episodes, version-aware resume | Server | bk | episodes in 12.0 | bk | Version picker 8.5 ([blog](https://firecore.com/blog/infuse-85-smarter-streaming)) | — | — |
| Should | Watch together / SyncPlay | Server-coordinated | removed from new apps 2025 ([AA](https://www.androidauthority.com/plex-watch-together-removed-3529939/)) | SyncPlay built-in ([AA](https://www.androidauthority.com/jellyfin-syncplay-explained-3530437/)) | bk | — | — | Robustness is a differentiator ([syncplayv2](https://github.com/kontell/jellyfin-plugin-syncplayv2)) |
| Should | Remote control of other clients / casting | Server relays sessions; Cast receiver | Companion (bk) | Sessions remote (bk) | bk | AirPlay (bk) | — | gap |
| Should | Downloads/offline + progress sync back | Server accepts offline playstate | bk | mobile (bk) | bk | bk | Offline Downloads ([Channels](https://getchannels.com/live-tv/)) | see state-sync note |
| Should | Collections, playlists, recently added, cast & crew | Server | yes | yes (12.0 perf) | yes | yes | Collections, Playlists | [12.0](https://jellyfin.org/posts/jellyfin-release-12.0/) |
| Should | Live TV: HDHomeRun + M3U/XMLTV, guide, Series Pass DVR, commercial markers | Server | yes (bk) | yes, improved 12.0 | bk | — | benchmark | [Channels](https://getchannels.com/live-tv/) |
| Should | "Still watching?" prompt; mark played at credits | Server (credits marker) + client | marker-based played ([Plex](https://support.plex.tv/articles/credits-detection/)) | still-watching 12.0 | — | — | — | — |
| Delight | Extras/trailers/deleted scenes on detail page | Server metadata | bk | bk | bk | 8.4 | — | [Infuse 8.4](https://firecore.com/blog/infuse-84-extras-intros-and-favorites) |
| Delight | Local, private "because you watched" recommendations | Server | Discover (disliked) | pluggable per library 12.0 | bk | — | — | — |
| Delight | Virtual channels / shuffle / multiview / sports seek | Server (virtual channels) + client | bk | — | — | — | yes | [Channels](https://getchannels.com/live-tv/) |
| Delight | Frame-step, mid/post-credits jump, theme music | Client / server markers | post-credits jump | frame step 12.0 | bk | — | — | — |
| Delight | Server-side transcode-on-demand picker from direct-play clients | Server | — | — | — | 8.5 long-press | — | [Infuse 8.5](https://firecore.com/blog/infuse-85-smarter-streaming) |

### Inferences
- **Highest leverage for a Jellyfin-compatible Rust server:** (1) fast home-row and Next Up queries, (2) Media Segments filled automatically (chapters, audio fingerprint, community DB) and shared between peer servers by content hash, (3) native trickplay generation using keyframe mode, (4) complete, correct MediaStream metadata (DV profile, frame rate, Atmos and DTS:X flags), (5) SyncPlay and session relay, (6) per-user, per-series track preferences, (7) Live TV with commercial segments. Most client-visible "nice stuff" then comes free in existing Jellyfin clients and Infuse.
- Things a server *cannot* fix: whether a client implements match-content, DV output, passthrough, subtitle styling or audio delay. Recommend specific clients per platform: Infuse on Apple TV, the official Jellyfin Android TV client or Kodi + Jellyfin on Android TV (bk), and web. Test them against the server.

### Gaps
- The "bk" cells were not verified with sources in this session. The report writer should treat them as plausible but unconfirmed.
- Kodi and the Netflix/Apple TV app columns were omitted for lack of fetched sources.
