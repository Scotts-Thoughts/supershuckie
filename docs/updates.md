# Updates

Super Shuckie (Windows) can update itself from this repository's GitHub releases, and switch to
any released version, or any version it kept, from the **Updates** menu.

## Publishing a release

`.github/workflows/release.yml` builds and publishes a release when a version is pushed:

- a commit whose message starts with the version: `v0.5.2 - fixes for play together` (only the
  newest commit of a push is looked at), or
- a tag named like the version: `git tag v0.5.2 && git push origin v0.5.2`, or
- **Actions › Release › Run workflow** with a version, which tags the chosen branch's newest commit.

The version must be `MAJOR.MINOR.PATCH`. A version that already has a release is not built again.
The workflow runs `scripts/ci-build-release.ps1` on a Windows runner: it writes the version into
`Cargo.toml` (so the exe's own version always matches its release, whatever the pre-commit hook
bumped it to), builds mGBA, melonDS (patched, LTO, no PGO), Azahar (cloned at the commit pinned in
the script, patched with `azahar-rs/patches`), then the static exe with
`-DSUPERSHUCKIE_RELEASE_BUILD=ON`. It takes roughly an hour or more.

The release `v<version>` gets three files:

- `supershuckie.exe`, which the Updates menu downloads (the name matters),
- `supershuckie-<version>-win64.zip`, the exe with `libraries-and-attributions/`, for people, and
- `supershuckie-<version>-symbols.zip`, holding `supershuckie.debug`: the exe's symbols.

The exe is stripped (about 71 MB instead of 134 MB), so nobody downloads symbols they don't use.
To debug a release, unzip `supershuckie.debug` next to the exe; gdb finds it there by itself.

Its notes are the commit message after its first line plus the commit subjects since the previous
release. Drafts, pre-releases and releases without `supershuckie.exe` are never offered.

When Azahar moves forward, update `-AzaharCommit` in `scripts/ci-build-release.ps1` to the
commit the patches apply to.

## In the app

- On start, unless silenced, Super Shuckie asks GitHub for the releases (one request; nothing is
  sent but the request). If one is newer than the running version, it asks, once no game is running:
  - **Update now**: download, switch, restart.
  - **Remind me later**: not asked again for 24 hours.
  - **Skip this version**: not asked again until a newer version comes out.
  - **Don't check for updates when Super Shuckie starts** (checkbox): never asked again.
- **Updates › Check for updates now** always checks, also when silenced or the version was skipped.
- **Updates › Update to <version>…** appears when a newer version exists (the menu title says
  "Updates (new!)" unless it was skipped).
- **Updates › Go back to the previous version** switches back to what ran before the last switch.
- **Updates › Roll back or switch to version** lists every release and every kept version.
- **Updates › Check for updates when Super Shuckie starts** silences or re-enables the launch check.
- **Updates › Open the kept versions folder**.

## How switching works

1. The running exe is copied into `versions\` next to it, as `supershuckie-<version>.exe` (or
   `supershuckie-<version>-own-build.exe` for an exe not made by the release workflow, since its
   version number may also belong to a different, released exe). Nothing changes if that fails.
2. The chosen version comes from `versions\` if it is there; otherwise it is downloaded into
   `versions\` and checked against the size and SHA-256 GitHub lists for the file.
3. Super Shuckie closes normally (settings, SRAM and a replay being recorded are saved; a Play
   Together session asks first). Once the window is gone, the exe is renamed to `<name>.old`, the
   chosen version is copied to its name, and it is started in the same working folder, so it
   finds the same `UserData`. The next start deletes the `.old` file.

`UserData` is never touched. Older versions may not read replays or save states written by newer
ones, which the confirmation says when rolling back. Kept versions are about 71 MB each (a kept own build can be twice that, since it still has its
symbols); delete the ones you don't want from the versions folder.

Settings (custom settings in `UserData/settings.json`): `update_check_on_launch` ("0" when
silenced), `update_skipped_version`, `update_remind_after` (Unix time), `update_previous_version`,
`update_installed_version` (written just before switching; the new version clears it).

Building it elsewhere: `-DSUPERSHUCKIE_UPDATE_REPO=owner/repo` points the menu at another
repository's releases (the workflow passes its own repository). The menu is Windows-only (WinHTTP).
