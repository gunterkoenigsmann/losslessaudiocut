# Releases

A release is a git tag `vX.Y.Z`. Pushing it starts
[`release.yml`](../.github/workflows/release.yml), which builds, tests and
publishes the packages below to the GitHub release of that tag. The snap is
built separately (see `snap-publish.yml` and `snap.md` once the snap is
merged); `release.yml` starts the snap publishing when it is done.

## Packages

| Platform | Architectures | Files | Built on |
|---|---|---|---|
| Windows 10/11 | x64, arm64 | `losslessaudiocut-VERSION-windows-ARCH.zip` (unpack and run), `losslessaudiocut-VERSION-windows-ARCH-setup.exe` (installer) | `windows-latest`, `windows-11-arm` |
| macOS | arm64 (Apple silicon), x86_64 (Intel) | `losslessaudiocut-VERSION-macos-ARCH.dmg` | `macos-latest`, `macos-15-intel` |
| Linux | x86_64, aarch64 | `losslessaudiocut-VERSION-linux-ARCH.AppImage` | `ubuntu-24.04`, `ubuntu-24.04-arm` |

Plus `SHA256SUMS.txt` with the checksums of all of them.

Every package holds the GUI and the command line tool `lac`:

- Windows: `lac.exe` next to `losslessaudiocut.exe` (in the zip folder, or
  the installation folder).
- macOS: `losslessaudiocut.app/Contents/MacOS/lac`.
- Linux: run `./losslessaudiocut-*.AppImage --appimage-extract`; the tool is
  `squashfs-root/usr/bin/lac`.

What goes into each package, and why:

- **FFmpeg** is linked dynamically and shipped with the program: the DLLs of
  a [BtbN](https://github.com/BtbN/FFmpeg-Builds) build on Windows,
  Homebrew's FFmpeg and its libraries (copied by `dylibbundler`) on macOS,
  Ubuntu 24.04's on Linux (copied by `linuxdeploy`).
- **wxWidgets** is built by wxdragon and linked statically.
- **GTK 3** (Linux) is bundled by linuxdeploy's GTK plugin; glibc, the
  graphics drivers and ALSA come from the user's system.
- **The C/C++ runtime** on Windows is linked statically
  (`-C target-feature=+crt-static`), so no Visual C++ redistributable is
  needed.

The scripts doing the packaging are in [`packaging/`](../packaging) and run
on a developer machine as well (after `cargo build --release -p lac-gui -p
lac-cli`):

| Script | Output |
|---|---|
| `packaging/version.sh [REF]` | the version for a git ref; fails when a tag does not match `Cargo.toml` |
| `packaging/linux/appimage.sh VERSION OUTDIR` | AppImage (needs `rsvg-convert`, downloads linuxdeploy) |
| `packaging/macos/dmg.sh VERSION OUTDIR` | `.dmg` (needs Homebrew's `dylibbundler` and `librsvg`) |
| `packaging/windows/package.ps1 -Version V -Arch A -FfmpegDir D -OutDir O` | zip and installer (needs Inno Setup 6.3+) |
| `packaging/icons/generate.sh` | `losslessaudiocut.ico` from the SVG icon |

## Testing in CI

Before anything is uploaded, every package is smoke-tested on its runner,
from the package and outside the build tree: the command line tool runs
`info`, `cut` and `detect` on fixtures (`detect` decodes, so it proves
that the bundled FFmpeg has the MP3 and Speex decoders), and the GUI
must start and still run after 15-20 s. On Windows both the unpacked zip
and a silent installation are tested; on macOS the programs run from the
mounted disk image, and the packaging script fails if any library still
points into Homebrew.

The same workflow runs on every pull request that changes `packaging/` or
the workflow, and can be started by hand (Actions, *Release*, *Run
workflow*); then the packages are only kept as artifacts of the run, with a
development version such as `0.1.0-dev.20261104.b46df06`.

## Making a release

1. Set `version` in `[workspace.package]` of `Cargo.toml` to `X.Y.Z`, in a
   PR, and merge it.
2. Tag the merge commit on `main` and push the tag:
   ```sh
   git checkout main && git pull
   git tag -a vX.Y.Z -m "losslessaudiocut X.Y.Z"
   git push origin vX.Y.Z
   ```
   A tag that does not match `Cargo.toml` fails the workflow before
   anything is built.
3. The workflow creates the GitHub release with notes generated from the
   merged pull requests. To write the notes yourself, create the release
   for the tag first (it may be a draft); the workflow then only adds the
   files.
4. A tag with a suffix, such as `v1.2.0-rc1`, becomes a pre-release. It does
   not count as the latest release, so the snap's stable channel ignores
   it.

## Unsigned packages

The packages are not code-signed yet, so the systems warn on first start:

- **Windows**: SmartScreen says "Windows protected your PC"; click *More
  info*, then *Run anyway*.
- **macOS**: Gatekeeper refuses to open an app from an unidentified
  developer. Right-click (Control-click) the app, choose *Open*, then *Open*
  again; on macOS 15 and newer, allow it under *System Settings*, *Privacy &
  Security* instead. Or remove the quarantine flag:
  `xattr -dr com.apple.quarantine /Applications/losslessaudiocut.app`.
- **Linux**: no warning; make the AppImage executable (`chmod +x`).

## Known limitations

- Signing and notarization:
  [#60](https://github.com/gunterkoenigsmann/losslessaudiocut/issues/60).
- The AppImage needs glibc 2.39 (Ubuntu 24.04 or newer), the DMG the macOS
  version of the build runner or newer, and both bundle more of FFmpeg than
  we use; an own minimal FFmpeg build would fix all three:
  [#61](https://github.com/gunterkoenigsmann/losslessaudiocut/issues/61).
- No file associations yet ("Open with", double-click on an audio file),
  the macOS app does not yet handle Finder's open-documents event, and
  `losslessaudiocut.exe` has no embedded icon:
  [#62](https://github.com/gunterkoenigsmann/losslessaudiocut/issues/62).
