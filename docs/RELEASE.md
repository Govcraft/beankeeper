# Releasing the Beankeeper CLI

Cut a GitHub binary release when a CLI version is ready for users. `install.sh`
tracks the latest GitHub release, and AUR `bk-bin` tracks its packaged binaries;
neither automatically follows `main`. Keep [CHANGELOG.md](../CHANGELOG.md) current
as user-visible changes land, then release intentionally instead of letting
source and installed versions drift.

The tag must be `beankeeper-cli-vX.Y.Z`, with `X.Y.Z` exactly equal to
`[package].version` in `beankeeper-cli/Cargo.toml`. Only stable three-component
versions are supported by this procedure. The library version is independent.
Library tags (`beankeeper-vX.Y.Z`) and crates.io publishing belong to
[#44](https://github.com/Govcraft/beankeeper/issues/44), outside this procedure.

**The PR for #75 does not push a production tag.** After it merges, the maintainer
or PM cuts the catch-up release using the CLI version at that time. The examples
below use `0.21.0`; read the manifest rather than assuming it is still current.

## Prepare and confirm the release commit

Use a clean checkout of the intended merged `main` commit. Confirm that its CLI
manifest version and changelog section describe what will ship. Bump the CLI
version for subsequent releases as appropriate, update `Cargo.lock` and the
agent skill version in `docs/beankeeper/SKILL.md`, and curate the matching
`## [X.Y.Z]` changelog section. A catch-up release may use the existing version.
Before publishing, replace that section's `Unreleased` date with the intended
release date and remove any provisional catch-up wording. Merge preparation
changes before tagging.

```sh
git fetch origin
git status --short                 # must print nothing, including untracked files
git rev-parse HEAD origin/main      # both lines must identify the intended commit
```

CI runs formatting, Clippy with warnings denied, and workspace tests on stable
Rust for Linux and macOS, plus a Rust 1.91.0 compatibility check. Confirm those
checks passed for the exact commit being released. To repeat the quality checks
locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Build a fresh binary from that same tree and run the offline preflight:

```sh
cargo build --locked -p beankeeper-cli
scripts/check-release.sh --tag beankeeper-cli-v0.21.0
```

The script reads the CLI manifest, requires a stable `X.Y.Z` version and an exact
`## [X.Y.Z]` changelog heading, checks the supplied tag, and executes `bk --version`
to require exactly `bk X.Y.Z`. It defaults to `target/debug/bk`, honoring
`CARGO_TARGET_DIR` when set. Pass a binary explicitly for release or other builds:

```sh
cargo build --release --locked -p beankeeper-cli
scripts/check-release.sh --tag beankeeper-cli-v0.21.0 "${CARGO_TARGET_DIR:-target}/release/bk"
python3 -B -m unittest discover -s scripts/tests -v
```

The preflight itself never builds, downloads, contacts GitHub, or uses AUR or
crates.io credentials. Building beforehand may need to download dependencies.
It checks version agreement, not source provenance: the clean tree, fresh build,
and exact commit confirmation above are still required. It does not require the
proposed tag to exist yet. `--metadata-only` skips the executable check so CI can
check the manifest and changelog on every PR and push to `main` without a build.

## Tag and push

Only the maintainer or PM performs this step after preparation has merged and
checks pass. Recheck the working tree is clean, then create a signed tag on the
confirmed commit and push only that tag:

```sh
git tag -s beankeeper-cli-v0.21.0 -m 'bk 0.21.0'
git push origin refs/tags/beankeeper-cli-v0.21.0
```

Do not move an existing published tag. Correct an already published release with
a new version and tag.

## Follow the workflow

[release.yml](../.github/workflows/release.yml) runs on pushes of
`beankeeper-cli-v*` tags:

1. Preflight validates the tag against the checked-out CLI manifest and
   changelog, runs the offline script tests, builds `bk` with locked dependencies,
   and verifies its reported version. A failure blocks the platform builds.
2. Four builds produce `bk` for x86_64 and aarch64 on Linux GNU and macOS. Linux
   aarch64 uses the installed cross-compilation toolchain.
3. Each archive is `bk-X.Y.Z-TARGET.tar.gz`, containing a directory of the same
   name with `bk`, `README.md`, `LICENSE-MIT`, and `LICENSE-APACHE`.
4. The release job collects the four archives, computes `checksums-sha256.txt`,
   and creates the GitHub release with generated notes. Edit those notes to link
   to the curated changelog at the release tag.
5. The AUR job downloads the published checksums, updates `PKGBUILD` and
   `.SRCINFO` for `bk-bin`, and pushes to AUR over SSH.

Watch the Release workflow in GitHub Actions and inspect the resulting release:

```sh
gh run list --workflow release.yml --limit 5
gh release view beankeeper-cli-v0.21.0
```

AUR requires repository secret `AUR_SSH_KEY` with push access to `bk-bin`. If it
is missing or invalid, the SSH clone or push fails. The GitHub release has
already been created and remains available to `install.sh`, while AUR stays at
its previous version. Treat that as an AUR failure, inspect the job logs, restore
the credential through the maintainer, and rerun the failed AUR job. There is no
automatic fallback or alternate credential. Do not retag to retry AUR.

## Verify what users install

Check that the release has all four archives and its checksum file, and that it
is designated the latest stable GitHub release. Verify both the default and
pinned installer paths in separate temporary directories so an existing `bk`
installation does not affect the result:

```sh
latest_dir=$(mktemp -d)
BK_INSTALL="$latest_dir" sh install.sh
"$latest_dir/bk" --version

pinned_dir=$(mktemp -d)
BK_INSTALL="$pinned_dir" BK_VERSION=0.21.0 sh install.sh
"$pinned_dir/bk" --version
```

Immediately after this release becomes latest, both should print `bk 0.21.0`.
`BK_VERSION` is the bare CLI version, without `v` or the tag prefix. The installer
downloads from `beankeeper-cli-v${VERSION}`, verifies SHA-256, installs `bk`, and
checks the installed version. Its PATH warning is expected for a temporary
directory; execute the explicit paths shown above. Remove the temporary
directories afterward. Confirm the AUR package's `pkgver` and Linux checksums
also match the release before announcing the AUR update.
