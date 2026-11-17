# The Python package

`pip install losslessaudiocut` installs the command line tool `lac` and a
Python API, both on top of the same Rust code as the GUI. It exists for
people and programs that can install a Python package but not a program:
AI command line clients in particular install from PyPI readily and avoid
system installers. The user documentation is the package's README,
[`crates/lac-python/README.md`](../crates/lac-python/README.md), which
PyPI shows.

## Layout

| Path | What |
|---|---|
| `crates/lac-python/src/lib.rs` | The extension module `losslessaudiocut._native` (PyO3). Thin: plain Python values in and out, the work in `lac-app` with the interpreter lock released. |
| `crates/lac-python/python/losslessaudiocut/` | The API users see: `info`, `cut`, `detect`, `decode`, result dataclasses, time and range conversion, the `lac` entry point (`_cli.py`, which runs `lac_cli::run` in-process). |
| `crates/lac-python/pyproject.toml` | Package metadata for maturin. The version comes from `Cargo.toml`. |
| `crates/lac-python/tests/` | pytest tests, run against an installed wheel. |
| `packaging/python/build-ffmpeg.sh` | Builds the minimal static FFmpeg of the Linux and macOS wheels. |
| `packaging/python/ffmpeg-version` | The FFmpeg release the wheels bundle. |
| `packaging/python/build-linux-wheel.sh` | The Linux build inside the manylinux_2_28 container. |
| `packaging/python/stage-wheel.sh` | Copies the license files into the package before a build. |
| `packaging/python/pypi_version.py` | Release tag to PEP 440 version, and the next post-release. |
| `.github/workflows/python.yml` | Builds, tests and publishes the wheels. |
| `.github/workflows/ffmpeg-update.yml` | Weekly: new FFmpeg point release, rebuild and republish. |

`lac-cli` is a library plus a two-line `main.rs`, so the package's `lac`
is the same program. Neither links the sound output: `lac-app`'s Cargo
feature `audio-output` (and `lac-playback`'s `device`) is only enabled by
the GUI, so the wheel needs no ALSA.

## FFmpeg in the wheels *(decided, issue #61 exception)*

The desktop packages use the system's FFmpeg so that the distribution's
security updates reach it (issue #61). A wheel cannot rely on a system
FFmpeg (pip installs no system libraries), so the wheels bundle it, and
the update duty moves to us:

- **Minimal**: Linux and macOS link FFmpeg statically, configured with
  `--disable-everything` plus the audio decoders `lac-decode` maps codecs
  to, no demuxers, parsers, encoders, filters, network or external
  libraries. Our own demuxers feed the decoders, so FFmpeg's demuxers,
  where many advisories land, are not even in the wheel. LGPL build (no
  `--enable-gpl`), license and source note in `losslessaudiocut/ffmpeg/`.
- **Windows** uses BtbN's LGPL shared build of the same release branch
  (`ffmpeg-nX.Y-latest-…-lgpl-shared`), rebuilt daily from that branch, its
  DLLs copied into the wheel by delvewheel. Building FFmpeg with MSVC is
  not worth it for one platform.
- **Updates**: `ffmpeg-update.yml` checks FFmpeg's release tags every
  Sunday. A new point release of the pinned branch (FFmpeg's point
  releases are mostly security fixes) becomes a PR that changes
  `ffmpeg-version`; when all wheels build and pass their tests with it the
  PR is merged and the newest release tag is republished as a
  post-release, `X.Y.Z.postN`, with the same code and the new FFmpeg. Users
  get it with `pip install -U losslessaudiocut`; pip never updates on its
  own. A new release branch (9.0 to 9.1) opens an issue instead, because
  it may need a newer `ffmpeg-next`.
- `losslessaudiocut.ffmpeg_version()` reports the bundled version.

No video decoders: the Python API has no video preview, and a file with a
video track is cut the same (the video is copied, never decoded).

## Building locally

Linux or macOS, with Rust, a C compiler, nasm (x86) and libclang:

```sh
packaging/python/build-ffmpeg.sh /tmp/ffmpeg
packaging/python/stage-wheel.sh /tmp/ffmpeg/share/doc/ffmpeg
cd crates/lac-python
python -m venv .venv && . .venv/bin/activate
pip install maturin pytest
FFMPEG_DIR=/tmp/ffmpeg maturin develop --release   # or: maturin build
cd ../.. && python -m pytest crates/lac-python/tests packaging/python
```

Without `FFMPEG_DIR` the build links the system's FFmpeg (pkg-config), like
the other crates; fine for development, not for a wheel to share.
`cargo build --workspace` builds the extension module too, without
linking libpython (`.cargo/config.toml` sets `PYO3_BUILD_EXTENSION_MODULE`).

## Releasing

A release tag `vX.Y.Z` publishes version `X.Y.Z` to PyPI along with the
other packages (`docs/release.md`); `vX.Y.Z-rcN` publishes the
pre-release `X.Y.ZrcN`, which pip only installs on request. Publishing
uses PyPI's trusted publishing: no token is stored in the repository.

Set up once, by the owner of the PyPI project:

1. On pypi.org, *Your projects* (or, before the first upload, *Publishing*
   for a pending publisher): add a trusted publisher of type GitHub with
   owner `gunterkoenigsmann`, repository `losslessaudiocut`, workflow
   `python.yml`, environment `pypi`.
2. On GitHub, *Settings*, *Environments*: create the environment `pypi`
   (optionally with required reviewers, so that each upload waits for a
   click).

`python.yml` can also be started by hand (Actions, *Python package*, *Run
workflow*): without *publish* it only builds and tests the chosen branch;
with *publish* it republishes the newest release tag (or the one given)
as the next post-release, with the FFmpeg of the chosen branch.
