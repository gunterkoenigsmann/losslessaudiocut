---
name: add-container-format
description: Step-by-step checklist for adding lossless cut support for a new audio container or codec (demuxer, cut planner, muxer, fixtures, equivalence tests). Use when implementing or extending WAV, FLAC, MP3, ADTS, M4A, Ogg Vorbis/Opus or Speex support.
---

# Adding a container format

Work on a dedicated feature branch (`feature/<format>-support`) and finish with
a PR (see the `feature-pr` skill). Read the per-format table in
`docs/architecture.md` section 5 first.

## 1. Research the format, write it down

Before coding, collect in a module-level `//!` doc comment of the new
`lac-container/src/<format>/mod.rs`:

- Spec references (document name, section numbers).
- Packet/frame layout and how to find packet boundaries (sync word, length
  field, page segmentation).
- **Granularity**: samples per packet (fixed or variable).
- **Pre-roll**: how many preceding packets a decoder needs to produce correct
  output for the first kept packet, and *why* (bit reservoir, MDCT overlap,
  LPC state). If it is data-dependent (MP3 `main_data_begin`), describe how to
  compute it per packet.
- **Trimming metadata**: can the container express "drop *n* leading/trailing
  samples"? Which field (LAME tag, `elst`, granule position, Opus `pre_skip`)?
- Header fields that must be rewritten on export (sizes, sample counts, CRCs,
  checksums, seek tables, serial numbers, frame numbers).
- Where tags live (ID3v2, Vorbis comment, `ilst`, `LIST/INFO`) and which
  fields map to title, artist, album, album artist, track number, date,
  genre, comment and cover.

## 2. Demuxer -> `PacketIndex`

- Parse from `Read + Seek`; no decoding.
- Each `PacketInfo` records byte offset, byte length, first sample and sample
  count. `first_sample` is the presentation position *after* encoder delay.
- Malformed data returns an error with the byte offset; it never panics.
- Unit tests on hand-crafted byte buffers: one valid packet, truncated packet,
  bad sync, bad CRC, zero-length stream.

## 3. `CutCapabilities` and `CutPlanner`

- Fill `CutCapabilities` honestly; the UI uses it to show where a cut will
  really land.
- `CutPlanner::plan` maps a requested sample range to
  `CutPlan { packets, trim_start, trim_end, actual }`, including pre-roll.
- Property test (`proptest`): for random ranges, `actual` contains the
  requested range when trimming is unsupported, equals it when trimming is
  supported, and `packets` never leaves the index bounds.

## 4. Muxer

- Copy packet payloads **verbatim** (lossy codecs must never be re-encoded).
- Rewrite every header field listed in step 1; recompute CRCs.
- Write trimming metadata from the `CutPlan` when supported.
- Write the track's `Tags` in the container's native tag format (see the
  tag table in `docs/architecture.md` section 5) and read the source's tags
  in the demuxer so they can be inherited. Tag round-trip gets its own test.
- A segment list with several segments must produce one valid stream; if the
  format cannot join segments with different codec parameters, return an
  error rather than writing a broken file.

## 5. Fixtures and equivalence tests

Use the `test-fixtures` skill to add 2-5 s fixtures (mono and stereo, at least
two sample rates, and for variable-block codecs one with mixed block sizes).

Integration tests in `lac-container/tests/<format>.rs`:

- Golden rule: `decode(cut(f, r)) == decode(f)[plan.actual]`, bit for bit, for
  cuts at the start, the end, the middle, on a packet boundary, one sample
  after a boundary, and a zero-length range.
- Round trip: cutting the full range reproduces the same decoded samples.
- Independent check: if `ffmpeg` is installed, decode the export with it and
  compare sample count and content (skip with a message, not a failure, when
  the tool is missing locally; CI must have it).

## 6. Wire up and document

- Register the format in the format probe (extension + magic bytes).
- Update the per-format table in `docs/architecture.md` with the achieved
  exactness.
- Mention any limitation in the PR description.
