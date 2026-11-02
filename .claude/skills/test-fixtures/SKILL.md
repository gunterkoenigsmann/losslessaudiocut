---
name: test-fixtures
description: How to create, name and commit small audio test fixtures and how to verify that a cut is lossless with independent tools. Use when a test needs an audio file, when adding a format, or when checking an exported file by hand.
---

# Test fixtures and lossless verification

## Principles

- Fixtures are **generated**, not recorded: every file in `tests/fixtures/`
  must be reproducible by `tests/fixtures/generate.sh`. Add new fixtures by
  extending that script, then run it and commit both the script and output.
- Keep each fixture short (2-5 s) and small (under ~200 kB). Total fixture
  size should stay well below 10 MB.
- Use deterministic signals so failures are reproducible and visible in a
  waveform: sine sweeps, a tone with a silent gap (useful later for silence
  detection), and seeded noise. Avoid pure silence-only files except for
  explicit edge-case tests.
- Pin encoder options explicitly (bitrate, sample rate, channels, block size)
  so a newer encoder version does not change behaviour silently.

## Naming

`<signal>_<rate>_<channels>.<ext>`, for example `sweep_44100_stereo.mp3`,
`gap_16000_mono.spx`. Add a suffix for special cases: `_vbr`, `_noxing`,
`_adts`, `_mixedblocks`.

## Generating with ffmpeg

```sh
# 3 s stereo sine sweep 100 Hz..8 kHz
ffmpeg -y -f lavfi -i "aevalsrc='sin(2*PI*(100+1317*t)*t)|sin(2*PI*(100+1317*t)*t)':s=44100:d=3" \
       -c:a libmp3lame -b:a 128k sweep_44100_stereo.mp3

# tone, 1 s silence, tone (for silence/track detection tests)
ffmpeg -y -f lavfi -i "sine=f=440:d=1" -f lavfi -i "anullsrc=r=44100:cl=mono:d=1" \
       -f lavfi -i "sine=f=660:d=1" -filter_complex "[0][1][2]concat=n=3:v=0:a=1" \
       -c:a flac gap_44100_mono.flac
```

Typical encoders: `pcm_s16le` (WAV), `flac`, `libmp3lame`, `aac` (`.m4a` and
`-f adts` for `.aac`), `libvorbis`, `libopus`, `libspeex` (`.spx`). If an
encoder is missing in ffmpeg, note it in the script and skip that fixture
with a clear message.

## Damaged and hostile fixtures

Real recordings are messy, so the generator also derives broken variants from
clean files (with a small Rust or Python helper committed next to the
script, never by hand-editing bytes):

- `_badgranule`: Ogg with random granule positions (large start offset,
  jumps, values going backwards), as written by many radio stream captures.
  Tests assert that the computed timeline is identical to the clean file's.
- `_chained`: two or more Ogg logical streams concatenated, each with its own
  comment header.
- `_midstream`: Ogg starting with a partial page, and a variant with a page
  whose CRC is wrong; the demuxer must resync and report, not fail.
- `_truncated`: any format cut off in the middle of a packet.

## Verifying a cut by hand

Decode both files to raw PCM with an independent decoder and compare:

```sh
ffmpeg -v error -i source.mp3 -f s16le -ac 2 source.pcm
ffmpeg -v error -i cut.mp3    -f s16le -ac 2 cut.pcm
# compare cut.pcm with the expected byte range of source.pcm
cmp cut.pcm <(tail -c +$((START*4+1)) source.pcm | head -c $(( (END-START)*4 )))
```

(4 bytes per frame for 16-bit stereo.) For FLAC also run `flac --test cut.flac`
(checks CRCs and the STREAMINFO MD5). For Ogg, `ogginfo` reports page and
granule errors. Be aware that ffmpeg honours trimming metadata (LAME tag,
`elst`, granule positions); a player that ignores it will play a few extra
milliseconds, which is expected and not a bug.

## In automated tests

- Load fixtures with a helper that resolves paths relative to
  `env!("CARGO_MANIFEST_DIR")`.
- External-tool checks must detect a missing tool and skip with a printed
  message locally; CI installs the tools so the checks always run there.
