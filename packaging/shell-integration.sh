#!/usr/bin/env bash
# Writes the manpages and shell completions of losslessaudiocut into PREFIX,
# in the layout of /usr/share (or ~/.local/share):
#
#   packaging/shell-integration.sh LAC PREFIX [NAME]
#
#   PREFIX/share/man/man1/lac.1, lac-cut.1, ..., losslessaudiocut.1
#   PREFIX/share/bash-completion/completions/NAME
#   PREFIX/share/zsh/site-functions/_NAME
#   PREFIX/share/fish/vendor_completions.d/NAME.fish
#
# LAC is the built `lac` binary, which generates all of them from its own
# command line definition (`lac manpages`, `lac completions`). NAME is the
# command the completions are for, `lac` unless the package calls it
# something else (the snap: `losslessaudiocut.cli`).
#
# Used by the AppImage, DMG and release tarball (packaging/linux/appimage.sh,
# packaging/macos/dmg.sh, .github/workflows/release.yml).
set -euo pipefail

lac="$1"
prefix="$2"
name="${3:-lac}"

share="$prefix/share"
mkdir -p "$share/bash-completion/completions" "$share/zsh/site-functions" \
    "$share/fish/vendor_completions.d"
"$lac" manpages "$share/man/man1"
"$lac" completions bash --bin-name "$name" > "$share/bash-completion/completions/$name"
"$lac" completions zsh --bin-name "$name" > "$share/zsh/site-functions/_$name"
"$lac" completions fish --bin-name "$name" > "$share/fish/vendor_completions.d/$name.fish"
find "$share" -type f | sort
