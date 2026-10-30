# flicks-container

Bit-level I/O (`bitstream`), the ISO BMFF / MP4 / MOV demuxer and muxer (`isobmff`) and the
Matroska / WebM demuxer and muxer (`matroska`).

Copied from craft-libs `crates/media` at 92d392c (see `THIRD_PARTY_NOTICES`). Implemented from the
published container specifications; ffmpeg is used only as a test oracle.

The stateless HLS writer follows ISO/IEC 14496-12:2022 and 14496-15:2022,
H.264:2021 / H.265:2021 SPS timing, and ETSI TS 102 366 V1.4.1 Annex F (Dolby entries).
