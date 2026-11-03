# losslessaudiocut

A lossless audio cutter in Rust: delete parts of a recording, join the rest
or split it into tracks, without ever re-encoding lossy audio.

Status: early development. WAV, FLAC, Ogg (Vorbis, Opus, Speex) and MP3
cutting work from the command line. The wxWidgets GUI shows the waveform
(zoom and scroll); cutting from the GUI and AAC follow. See
[`docs/architecture.md`](docs/architecture.md) for the design.

## GUI

```sh
cargo run --bin losslessaudiocut -- recording.mp3
```

Mouse wheel zooms at the pointer, Shift + wheel (or a horizontal wheel)
scrolls; View menu: zoom in (Ctrl++), out (Ctrl+-), to fit (Ctrl+0).

## Command line

```sh
cargo run --bin lac -- info recording.wav
cargo run --bin lac -- cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
cargo run --bin lac -- cut recording.wav -o out.wav --keep 0:10..3:45
```

Times are seconds (`90.5`), `m:ss`, `h:mm:ss` or `@N` for sample frame N.

## Development

Building needs the FFmpeg development files (FFmpeg 5.1 or newer: libavcodec,
libavformat, libavutil) and libclang, e.g. `apt install libavcodec-dev
libavformat-dev libavutil-dev clang pkg-config` or `brew install ffmpeg
pkg-config`. Only FFmpeg's decoders are used.

The GUI uses [wxdragon](https://crates.io/crates/wxdragon), which downloads
and builds wxWidgets itself (the first build takes several minutes). It needs
CMake and a C++ compiler everywhere, and on Linux the GTK 3 development files
(`apt install cmake ninja-build libgtk-3-dev`).

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Some tests cross-check results with `ffmpeg`, `flac`, `oggdec`
(vorbis-tools) and `mpg123` if they are installed; set `LAC_REQUIRE_TOOLS=1` to make missing tools an error.
Test fixtures are regenerated with `crates/lac-container/tests/fixtures/generate.sh`. See
[`AGENTS.md`](AGENTS.md) for conventions.

## License

GPL-3.0-or-later
