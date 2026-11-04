# Snap package

losslessaudiocut is published on the Snap Store as
[`losslessaudiocut`](https://snapcraft.io/losslessaudiocut). The recipe is
[`snap/snapcraft.yaml`](../snap/snapcraft.yaml); it contains the GUI
(`losslessaudiocut`) and the command line tool (`losslessaudiocut.cli`, the
`lac` binary).

## What gets built where

| Workflow | When | What |
|---|---|---|
| [`snap.yml`](../.github/workflows/snap.yml) | every push to `main`; PRs that touch `snap/`, a `Cargo.toml` or `Cargo.lock`; by hand | builds the snap, installs it, runs the CLI on a fixture and starts the GUI under Xvfb. The `.snap` is kept as the run's `snap` artifact. Never uploads. |
| [`snap-publish.yml`](../.github/workflows/snap-publish.yml) | Mondays 03:17 UTC; by hand | builds the newest `main` and uploads it to **edge**, builds the newest GitHub release and uploads it to **stable** (skipped while there is no release). |

The weekly rebuild is what brings security fixes to users: the snap bundles
FFmpeg and ALSA from the Ubuntu 24.04 archive, and those only update when the
snap is rebuilt. GTK and the rest of the desktop stack come from the
`gnome-46-2404` snap, which the store updates on its own. Rust crates are
pinned by `Cargo.lock` and only change when a PR updates them (`cargo deny`
in CI flags crates with advisories).

### Version and grade

Set by the recipe from git:

- a commit with a tag `vX.Y.Z` builds version `X.Y.Z`, grade `stable`;
- any other commit builds e.g. `0.1.0+git20261104.1e4a3e4` (version from
  `Cargo.toml`, commit date and hash), grade `devel`.

The store only accepts grade `stable` on the candidate and stable channels,
so a release needs a tag (and a GitHub release, which is what the weekly job
looks for).

## First upload (by hand)

The store needs one upload by the name's owner before CI may publish.

1. Get a `.snap`: download the `snap` artifact of the latest
   [Snap workflow run](https://github.com/gunterkoenigsmann/losslessaudiocut/actions/workflows/snap.yml)
   on `main` and unzip it, or build it yourself with
   `sudo snap install snapcraft --classic` and `snapcraft pack` in the
   repository (needs LXD: `sudo snap install lxd && sudo lxd init --auto`).
2. Try it locally before uploading:
   ```sh
   sudo snap install --dangerous ./losslessaudiocut_*_amd64.snap
   losslessaudiocut             # open a file, play it, export a cut
   losslessaudiocut.cli --help
   ```
   Things only a desktop can check: sound comes out (PulseAudio or
   PipeWire), the file dialogs work, and files on USB sticks open after
   `sudo snap connect losslessaudiocut:removable-media`.
3. Upload it to edge:
   ```sh
   sudo snap install snapcraft --classic   # if not done yet
   snapcraft login
   snapcraft upload --release=edge losslessaudiocut_*_amd64.snap
   ```
   The first upload of a new snap may wait for a manual store review; the
   upload command prints the status, and the same is shown on
   <https://snapcraft.io/losslessaudiocut/releases>.
4. Fill in the store listing at
   <https://snapcraft.io/losslessaudiocut/listing>: category (Audio & Video),
   screenshots, and check icon and description. Summary, description,
   license and links come from `snapcraft.yaml`.

## Credentials for the weekly job

CI uploads with a store login that only the name's owner can create. It is
limited to this one snap and to uploading and releasing.

1. Export the login (it asks for the store password and 2FA):
   ```sh
   snapcraft export-login \
     --snaps=losslessaudiocut \
     --channels=edge,stable \
     --acls=package_access,package_push,package_update,package_release \
     --expires="$(date -d '+1 year' +%Y-%m-%d)" \
     snapcraft-credentials.txt
   ```
2. Add the file's content as a repository secret named
   `SNAPCRAFT_STORE_CREDENTIALS`: on GitHub,
   [Settings → Secrets and variables → Actions → New repository secret](https://github.com/gunterkoenigsmann/losslessaudiocut/settings/secrets/actions/new),
   paste the whole file, or with the GitHub CLI:
   ```sh
   gh secret set SNAPCRAFT_STORE_CREDENTIALS \
     -R gunterkoenigsmann/losslessaudiocut < snapcraft-credentials.txt
   ```
3. Delete the file: `rm snapcraft-credentials.txt`.
4. Start the workflow once by hand (Actions → Snap publish → Run workflow)
   and check that edge got a new revision.

The login expires after the date given above; the weekly job then fails at
the publish step, and steps 1 to 3 renew it. Without the secret the job
still builds, but only warns instead of uploading.

## Making a release

1. Tag the commit `vX.Y.Z` (matching `version` in `Cargo.toml`) and create a
   GitHub release for the tag.
2. Run *Snap publish* by hand, or wait for Monday: the release goes to
   stable, `main` to edge.
