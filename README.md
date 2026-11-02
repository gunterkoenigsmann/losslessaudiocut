# losslessaudiocut

A lossless audio cutter in Rust: delete parts of a recording, join the rest
or split it into tracks, without ever re-encoding lossy audio.

Status: early development. WAV, FLAC and Ogg (Vorbis, Opus, Speex) cutting
work from the command line; MP3, AAC and the wxWidgets GUI follow. See
[`docs/architecture.md`](docs/architecture.md) for the design.

## Command line

```sh
cargo run --bin lac -- info recording.wav
cargo run --bin lac -- cut recording.wav -o out.wav --delete 1:00..1:30 --delete 5:00..
cargo run --bin lac -- cut recording.wav -o out.wav --keep 0:10..3:45
```

Times are seconds (`90.5`), `m:ss`, `h:mm:ss` or `@N` for sample frame N.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Some tests cross-check results with `ffmpeg`, `flac` and `oggdec`
(vorbis-tools) if they are installed; set `LAC_REQUIRE_TOOLS=1` to make missing tools an error.
Test fixtures are regenerated with `crates/lac-container/tests/fixtures/generate.sh`. See
[`AGENTS.md`](AGENTS.md) for conventions.

## License

GPL-3.0-or-later
