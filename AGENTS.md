# AGENTS.md

Guidance for AI coding agents (and humans) working on **losslessaudiocut**, a
Rust + wxWidgets audio editor that cuts AAC, Speex, Ogg (Vorbis/Opus), MP3,
WAV and FLAC recordings **without re-encoding lossy audio**.

The design lives in [`docs/architecture.md`](docs/architecture.md). Read it
before structural changes; if a change contradicts it, update the document in
the same PR and say so in the PR description.

## Golden rule

Kept audio must decode to **exactly** the same samples as the source. Every
change to cutting, muxing or demuxing must preserve this invariant and be
covered by a test that checks it:

```
decode(cut(file, range)) == decode(file)[actual_range]   // bit for bit
```

The one documented exception are **internal join points** of lossy codecs
(after a deleted part): there the first packet(s) decode with a different
predecessor state. Tests compare everything outside that window bit for bit
and check that the window is no longer than `docs/architecture.md`
(section "Join points inside a file") says for the codec. Speex and Opus in
Matroska or MP4 (joined in one stream without pre-roll) are the codecs whose
decoders never provably reach bit-exactness after a join; there the tests
require a difference below one 16-bit LSB after the window (decided in
issues #17 and #54).

Never "fix" a failing equivalence test by loosening the comparison. A lossy
codec is never re-encoded; FLAC/PCM boundary frames may be, because that is
itself lossless.

## Workflow

- **One branch per feature, each ending in a pull request.** Never commit
  directly to `main`. See the `feature-pr` skill in `.claude/skills/`.
- Keep a PR to one feature. Unrelated cleanups go into their own PR.
- A red test found by CI is a success of the process, not a failure: fix the
  cause, never skip, ignore or weaken the test.

## Commands

Run all of these before pushing (CI runs the same):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check   # advisories, licenses, sources; see deny.toml
```

The GUI crate needs CMake, a C++ compiler and on Linux GTK 3 development
files (`libgtk-3-dev`); wxdragon builds wxWidgets itself. `lac-decode` needs
FFmpeg's (libavcodec, libavformat, libavutil; FFmpeg 5.1 or newer) plus
libclang for bindgen. `lac-playback` uses `cpal`, which needs the ALSA
development files on Linux (`libasound2-dev`). If they are missing, use
`--exclude lac-gui` or `--exclude lac-decode` locally and mention it in the
PR.

## Layering

Cargo workspace, dependencies only point downwards (see the diagram in the
architecture doc):

```
lac-gui, lac-cli  ->  lac-app  ->  lac-analysis, lac-peaks, lac-playback
                                ->  lac-decode  ->  lac-container  ->  lac-core
```

- `lac-core` has no I/O and no external dependencies.
- Only `lac-gui` may depend on wxWidgets bindings. All UI logic (selection,
  zoom, snapping, undo) lives in view models in `lac-app` so it is testable
  without a display.
- Only `lac-decode` may depend on FFmpeg (`ffmpeg-next`), and only on its
  decoders (libavcodec). It is fed packets from our own demuxers, so the
  packet index stays the single source of truth.
- Container parsing and writing, cut planning and tag writing are our own
  code in `lac-container`. Never use FFmpeg's demuxers, muxers or stream copy
  for exporting; decoded PCM must never end up in an exported lossy file.

## Code conventions

- **Documentation**: doxygen-style `///` comments on every public item, and
  inline `//` comments for anything not self-explanatory (bit layouts, spec
  quirks, why a pre-roll count is what it is). Cite the spec section for
  format details, e.g. `// ISO/IEC 11172-3 2.4.1.7: main_data_begin`. Library
  crates use `#![deny(missing_docs)]`.
- **Time**: positions and durations are integer sample counts (`SamplePos`,
  `u64`) at the source sample rate. Floating point seconds exist only in the
  UI layer for display.
- **Errors**: `thiserror` enums in libraries, `anyhow` only in binaries. No
  `unwrap()`/`expect()` on data that comes from a file; malformed input must
  produce an error, never a panic.
- **No `unsafe`** outside FFI wrappers in `lac-decode`; each `unsafe` block
  gets a `// SAFETY:` comment.
- Parsers take `&[u8]` or `Read + Seek` so they can be unit-tested on
  hand-crafted byte buffers.
- **Keyboard shortcuts come from menus** (issue #99): give a command a
  menu item and put its shortcut after the label's `\t` (`"&Open…\tCtrl+O"`),
  so it is translated, shown next to the command and found without the
  manual. A toolbar button or control with a shortcut names the menu
  item's. No window catches keys itself (`on_key_down`, `on_char`,
  accelerator tables); `crates/lac-gui/src/hotkeys.rs` tests this and that
  no two menu items share a shortcut.
- **User-visible texts are translatable**: mark them with `tr`, `trf`,
  `tr_menu` or `n_` from `lac_app::i18n`. Don't edit `po/messages.pot` or
  add a test that `de.po` is complete; a weekly workflow
  (`translations.yml`) merges new texts into the `.po` files.

## Testing

- Unit tests next to the code (`#[cfg(test)] mod tests`).
- Property tests with `proptest` for cut planners and muxers: random ranges,
  check the golden-rule equivalence plus container validity.
- Fixtures: short generated files in `tests/fixtures/`, produced by the
  committed script `tests/fixtures/generate.sh`. See the `test-fixtures` skill.
- Where available, cross-check exported files with an independent decoder
  (`ffmpeg`, `flac --test`) so our demuxer and muxer cannot agree on a bug.
- Every bug fix starts with a test that reproduces it.
- Fuzzing: `fuzz/` holds cargo-fuzz targets for format detection and each
  demuxer/muxer; CI runs each for two minutes. Locally (nightly toolchain):
  `cd fuzz && cargo +nightly fuzz run <target> corpus/<target> ../crates/lac-container/tests/fixtures`.
  A crash becomes a unit test with the minimized input as a byte literal.

## Adding a format

Follow the `add-container-format` skill. In short: demuxer -> packet index ->
`CutCapabilities` -> `CutPlanner` -> muxer -> fixtures -> equivalence tests ->
fuzz target in `fuzz/` -> update the per-format table in `docs/architecture.md`.
