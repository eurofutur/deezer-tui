---
title: "FLAC seek: keep the header, skip the audio bytes"
author: Pyratt
date: 2026-10-29
branch: main
touches:
  - src/main.rs
---

## Decided
- A seek restarts the stream (`PlayTrackAt`) and drops decrypted bytes up to a target proportional to the position. FLAC was disabled because dropping from byte 0 also removed `fLaC` + STREAMINFO.
- `SeekSkip` (src/main.rs) buffers the FLAC header (`flac_header_len` walks the metadata blocks up to the last-block flag), keeps it, then drops [head, target). target = head + (len - head) * seek / duration. For MP3, head = 0, same as before.
- All the "FLAC seek is disabled" guards are gone (keyboard, mouse, MPRIS, quality switch, initial_ms).

## Tried and ruled out
- rodio `Sink::try_seek` on the decoder: StreamBuffer blocks until download and symphonia's FLAC seek wants the total length; doesn't fit the restart-pipeline architecture.

## Watch out
- Relies on symphonia-bundle-flac `parser.resync` (CRC check) to lock onto the next frame after the cut. Not tested on real audio from this session.
- The position is proportional to bytes, so it may be off by a few seconds in VBR FLAC.
