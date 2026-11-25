# Releasing Orbit

How a release reaches people: the bundles CI builds, the signed update
manifest each channel publishes, and how the app installs it. The updater
lives in `crates/orbit-app/src/update/`.

## Before the first release

Three placeholders keep a build from talking to anything until they are set:

| What | Where | Placeholder |
|---|---|---|
| Release signing key (public half) | `update/verify.rs`, `RELEASE_PUBLIC_KEY` | `REPLACE-WITH-THE-ORBIT-RELEASE-PUBLIC-KEY` |
| Manifest address | `update/mod.rs`, `MANIFEST_URL` | `https://updates.orbit.invalid/{channel}.json` |
| Documentation link (About) | `diagnostics/about.rs`, `DOCS_URL` | `https://docs.orbit.invalid/` |

With the placeholder key a build never checks or downloads anything:
Settings › Updates says "Off", and Check for updates explains why. `.invalid`
names never resolve, and About hides the docs link while it is one.

### Make the signing key

Releases are signed with [minisign](https://jedisct1.github.io/minisign/)
(Ed25519). Once, on a trusted machine:

```sh
minisign -G -p orbit-release.pub -s orbit-release.key
```

Put the second line of `orbit-release.pub` (it starts with `RW`) into
`RELEASE_PUBLIC_KEY`. Keep `orbit-release.key` and its password offline or in
the CI secret store; never commit it. Rotating the key means shipping one
release signed with the old key whose binary trusts the new one.

## Channels

| Channel | Gets | Version |
|---|---|---|
| `stable` | Tested releases | `0.4.0` |
| `beta` | The next release, a few weeks early | `0.5.0-beta.2` |
| `nightly` | Every day's build of `main` | `0.5.0-nightly.20261125` |

Versions are [semver](https://semver.org); the updater only ever installs a
version greater than the running one, so a pre-release (`-beta.N`,
`-nightly.YYYYMMDD`) sorts below its final release. Switching from nightly to
stable therefore waits for a stable release newer than the nightly in use. A
version that was installed and rolled back on this computer is not offered
again automatically (a manual check still offers it).

## The manifest

One JSON document per channel at `MANIFEST_URL` with `{channel}` replaced:

```json
{
  "version": "0.4.0",
  "channel": "stable",
  "pub_date": "2026-11-25T12:00:00Z",
  "notes": "## Orbit 0.4.0\n\n- Markdown release notes, shown before restarting and as What's new after.",
  "platforms": {
    "x86_64-pc-windows-msvc": {
      "url": "https://…/orbit-0.4.0-x86_64-pc-windows-msvc.zip",
      "sha256": "<lowercase hex SHA-256 of the file>",
      "size": 31457280,
      "signature": "untrusted comment: …\nRUQ…\ntrusted comment: orbit 0.4.0 x86_64-pc-windows-msvc\n…\n"
    },
    "aarch64-apple-darwin": { "…": "…" },
    "x86_64-apple-darwin": { "…": "…" },
    "x86_64-unknown-linux-gnu": { "…": "…" },
    "x86_64-unknown-linux-gnu.appimage": { "…": "…" }
  }
}
```

- **Platform keys** are target triples, as in the bundle names. An Orbit
  running from an AppImage (`$APPIMAGE` is set) looks for
  `<triple>.appimage` and replaces the AppImage file itself; otherwise Linux
  uses the tarball. A platform missing from a release simply isn't offered it.
- **`signature`** is the whole `.minisig` file. Its **trusted comment must be
  `orbit <version> <platform key>`**: it is covered by the signature, so an
  older signed bundle can't be replayed as a newer version or another
  platform's bundle.
- `size` is optional (progress before the server sends a length); `channel`
  and `pub_date` are informational.

The manifest itself needn't be signed: every byte installed is checked
against the SHA-256 and the signature, both made with the offline key.

## Cutting a release

1. Bump `version` in the root `Cargo.toml`, commit, tag `vX.Y.Z`, push the
   tag. `.github/workflows/release.yml` builds the bundles on every OS
   (`scripts/bundle.ps1`, `scripts/bundle.sh`) and attaches them to a GitHub
   release. Set `ORBIT_COMMIT` in CI if the checkout has no git history; About
   and crash reports show it.
2. Sign each bundle with the release key and the trusted comment above:

   ```sh
   v=0.4.0
   for f in dist/orbit-$v-*; do
     t=${f#dist/orbit-$v-}; t=${t%.zip}; t=${t%.tar.gz}
     minisign -S -s orbit-release.key -m "$f" -t "orbit $v $t"
   done
   ```

   minisign 0.10 and later sign the BLAKE2b-512 of the file (prehashed),
   which is the only form the updater accepts.
3. Write `<channel>.json` from the files: `sha256sum` (or
   `Get-FileHash -Algorithm SHA256`) for `sha256`, the `.minisig` contents
   for `signature`, the download address for `url`, and the release notes.
4. Upload the bundles first, then the manifest, so no Orbit ever reads a
   manifest whose files aren't there yet.

## What the app does

1. **Check** 15 seconds after start and every 6 hours (Settings › Updates ›
   Check automatically), or on Check for updates.
2. **Download** in the background (Download automatically) to
   `<data>/updates/downloads/`, over HTTPS only (plain HTTP only to
   loopback), following up to five redirects and never from HTTPS to HTTP.
3. **Verify** the SHA-256 and the minisign signature in one pass over the
   file; a mismatch deletes the download and says so.
4. **Stage**: unpack under `<data>/updates/staged/<version>/` (zip-slip and
   tar traversal refused) and find the payload: the folder holding
   `orbit-desktop`, the `Orbit.app` bundle, or the AppImage.
5. **Restart to update** appears in the title bar. The sheet shows the
   release notes and, when agents would stop, how many.
6. **Restart.** The app asks the restart host to get ready
   (`update::host::RestartHost`; the in-process engine needs nothing, a
   persistent host can drain or hand over), writes
   `<data>/updates/apply.json`, starts the helper (`orbit-desktop
   --orbit-apply-update <plan>`; on Windows a copy in the data folder) and
   quits.
7. **Swap.** The helper waits for the data folder's instance lock (the old
   Orbit has exited), moves every replaced path aside as `<name>.orbit-old`,
   moves the new one in (all or nothing), records `pending.json` and starts
   the new Orbit.
8. **Health.** Five seconds after its window opens, the new Orbit writes
   `updates/healthy`, removes the old copies and shows What's new. If it
   exits before that, the helper puts the old files back and starts the old
   Orbit, which says the update didn't start and won't retry that version on
   its own. If the helper is gone, the new Orbit rolls itself back after
   three starts that never became healthy.

Layouts: Windows and Linux tarballs replace the files beside
`orbit-desktop` (so `orbit` and `orbitd` update too); macOS replaces the whole
`.app` (it must be writable, so in `/Applications` for an admin user);
AppImages replace themselves.

## Testing updates locally

Never test against a published channel. Build two versions and serve a
fixture on loopback:

```powershell
$env:CARGO_TARGET_DIR = "…"
# The version to update to: bump the root Cargo.toml to 0.1.1, build, keep the exe, revert.
cargo build -p orbit-app; Copy-Item $env:CARGO_TARGET_DIR\debug\orbit-desktop.exe new\
# A signed manifest and bundle (test key) for it:
$env:ORBIT_FIXTURE_DIR = "fixture"; $env:ORBIT_FIXTURE_APP = "new\orbit-desktop.exe"
$env:ORBIT_FIXTURE_VERSION = "0.1.1"; $env:ORBIT_FIXTURE_BASE = "http://127.0.0.1:8765"
cargo test -p orbit-app make_update_fixture -- --ignored
python -m http.server 8765 --bind 127.0.0.1 --directory fixture
# The current version, from a copy (it replaces itself), on a scratch data folder:
$env:ORBIT_DATA = "scratch-data"
$env:ORBIT_UPDATE_URL = "http://127.0.0.1:8765/{channel}.json"
$env:ORBIT_UPDATE_PUBLIC_KEY = Get-Content fixture\public-key.txt   # debug builds only
$env:ORBIT_DEBUG_UPDATE = "sheet"   # or "restart" to install as soon as it's ready
install\orbit-desktop.exe
```

A bundle whose `orbit-desktop` exits at once (any program that does) tests
the rollback. `ORBIT_UPDATE_PUBLIC_KEY` is ignored by release builds;
`ORBIT_UPDATE_URL` (a mirror) is honored by all.

## Crash reports and diagnostics

Nothing is uploaded, ever; telemetry is none. A panic writes
`<data>/crashes/crash-<time>-<pid>.txt` (version, commit, system, backtrace,
the last log lines, redacted) and the next start offers View report, Copy
and Dismiss. The log is `<data>/logs/orbit-desktop.log` (2 MB, one rotation).
Help › Collect diagnostics (also in Settings › About and the palette) saves a
zip of the logs, crash reports, version and agent versions with tokens, API
keys, environment values, URL credentials and the home folder removed.
Release builds strip symbols, so backtraces name addresses; keep each
release's unstripped binary (or build with `strip = false`) to symbolize them.
