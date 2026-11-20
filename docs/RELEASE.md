# Releasing the Beankeeper CLI

Cut a GitHub binary release when a CLI version is ready for users. `install.sh`
tracks the latest GitHub release, and AUR `bk-bin` tracks its packaged binaries;
neither automatically follows `main`. Keep [CHANGELOG.md](../CHANGELOG.md) current
as user-visible changes land, then release intentionally instead of letting
source and installed versions drift.

The tag must be `beankeeper-cli-vX.Y.Z`, with `X.Y.Z` exactly equal to
`[package].version` in `beankeeper-cli/Cargo.toml`. Only stable three-component
versions are supported by this procedure. The library version is independent.
Use `beankeeper-vX.Y.Z` for library tags and `beankeeper-cli-vX.Y.Z` for CLI
tags going forward; do not create new bare `vX.Y.Z` tags or rewrite historical
tags. Library tags do not trigger the binary release workflow. See
[crates.io publishing](#cratesio-publishing) below for registry publishing and
the one-time CLI bootstrap.

CLI [0.23.0](https://github.com/Govcraft/beankeeper/releases/tag/beankeeper-cli-v0.23.0)
was released on 2026-11-20, including the changes tracked under 0.21.0 and
0.22.0. The examples below use the current source CLI version, `0.25.0`, for a
future release; they do not mean it has shipped. Read the manifest at release
time and substitute its version throughout.

## Prepare and confirm the release commit

Use a clean checkout of the intended merged `main` commit. Confirm that its CLI
manifest version and changelog section describe what will ship. Bump the CLI
version for subsequent releases as appropriate, update `Cargo.lock` and the
agent skill version in `docs/beankeeper/SKILL.md`, and curate the matching
`## [X.Y.Z]` changelog section. An unreleased source version may be used as-is
if it describes the intended release.
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
scripts/check-release.sh --tag beankeeper-cli-v0.25.0
```

The script reads the CLI manifest, requires a stable `X.Y.Z` version and an exact
`## [X.Y.Z]` changelog heading, checks the supplied tag, and executes `bk --version`
to require exactly `bk X.Y.Z`. It defaults to `target/debug/bk`, honoring
`CARGO_TARGET_DIR` when set. Pass a binary explicitly for release or other builds:

```sh
cargo build --release --locked -p beankeeper-cli
scripts/check-release.sh --tag beankeeper-cli-v0.25.0 "${CARGO_TARGET_DIR:-target}/release/bk"
cargo fetch --locked
python3 -B -m unittest discover -s scripts/tests -v
```

The preflight itself never builds, downloads, contacts GitHub, or uses AUR or
crates.io credentials. Building and fetching beforehand may download dependencies.
The packaging tests run offline and require `cargo fetch --locked` first.
It checks version agreement, not source provenance: the clean tree, fresh build,
and exact commit confirmation above are still required. It does not require the
proposed tag to exist yet. `--metadata-only` skips the executable check so CI can
check the manifest and changelog on every PR and push to `main` without a build.

## Tag and push

Only the maintainer or PM performs this step after preparation has merged and
checks pass. Recheck the working tree is clean, then create a signed tag on the
confirmed commit and push only that tag:

```sh
git tag -s beankeeper-cli-v0.25.0 -m 'bk 0.25.0'
git push origin refs/tags/beankeeper-cli-v0.25.0
```

Do not move an existing published tag. Correct an already published release with
a new version and tag.

## Follow the workflow

[release.yml](../.github/workflows/release.yml) runs on pushes of
`beankeeper-cli-v*` tags:

1. Preflight validates the tag against the checked-out CLI manifest and
   changelog, fetches locked dependencies, runs the offline script tests, builds
   `bk` with locked dependencies, and verifies its reported version. A failure
   blocks the platform builds.
2. Four builds produce `bk` for x86_64 and aarch64 on Linux GNU and macOS. Linux
   aarch64 uses the installed cross-compilation toolchain.
3. Each archive is `bk-X.Y.Z-TARGET.tar.gz`, containing a directory of the same
   name with `bk`, `README.md`, `LICENSE-MIT`, and `LICENSE-APACHE`.
4. The release job collects the four archives, computes `checksums-sha256.txt`,
   and creates the GitHub release with generated notes. Edit those notes to link
   to the curated changelog at the release tag.
5. The AUR job downloads the published checksums, updates `PKGBUILD` and
   `.SRCINFO` for `bk-bin`, and pushes to AUR over SSH.

After all four builds succeed, `publish-crates` authenticates to crates.io with
OIDC and publishes the library followed by the CLI, skipping versions already
published. It runs alongside the GitHub release job. A CLI authorization rejection
warns without failing the job while its trusted publisher awaits setup; other
publishing failures fail the job. See the bootstrap procedure below.

Watch the Release workflow in GitHub Actions and inspect the resulting release:

```sh
gh run list --workflow release.yml --limit 5
gh release view beankeeper-cli-v0.25.0
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
BK_INSTALL="$pinned_dir" BK_VERSION=0.25.0 sh install.sh
"$pinned_dir/bk" --version
```

Immediately after this release becomes latest, both should print `bk 0.25.0`.
`BK_VERSION` is the bare CLI version, without `v` or the tag prefix. The installer
downloads from `beankeeper-cli-v${VERSION}`, verifies SHA-256, installs `bk`, and
checks the installed version. Its PATH warning is expected for a temporary
directory; execute the explicit paths shown above. Remove the temporary
directories afterward. Confirm the AUR package's `pkgver` and Linux checksums
also match the release before announcing the AUR update.

## crates.io publishing

As of 2026-11-20, the `beankeeper` library is published at 0.7.0, but
`beankeeper-cli` has not been published. Its first publication and trusted-publisher
setup are still pending. Until then, use `install.sh`, AUR, or the Git source
install in the [README](../README.md#install) for the CLI. Neither
`cargo install beankeeper-cli` nor `cargo binstall beankeeper-cli` is available yet.

`beankeeper` publishes only through the `publish-crates` job in
[release.yml](../.github/workflows/release.yml), never with an API token. Its
crates.io settings require Trusted Publishing for all new versions from GitHub
repository `Govcraft/beankeeper`, workflow `release.yml`. The job has
`id-token: write` permission and uses `rust-lang/crates-io-auth-action@v1` to
obtain a temporary OIDC credential, with no stored crates.io secret. The action
revokes that credential when the job finishes. PR CI only runs dry runs; a
maintainer or PM triggers publishing by pushing the approved CLI release tag.

Keep the CLI's `beankeeper` dependency's `version` equal to the library's package
version, while retaining its `path` for local development. The crates have
independent versions; do not copy the CLI version into the library dependency.
Each manifest explicitly includes sources, tests, the shared README, and both
license texts. The license symlinks point to the repository's canonical files;
Cargo flattens them into regular files in the source archives.

On a clean release commit, use current stable Rust and run:

```sh
cargo fetch --locked
python3 -B -m unittest discover -s scripts/tests -v
cargo package -p beankeeper --list
cargo package -p beankeeper-cli --list
cargo publish --dry-run --workspace --locked
```

The last command packages and builds both crates in dependency order without
uploading. Cargo stages the library archive in a temporary local registry so the
CLI is verified even before the matching library version exists on crates.io.
This is the same check CI runs. Do not replace it with `--no-verify`, a path
patch, or a check that ignores failures. It needs network access for the registry
index and dependencies, but no publishing credential.

The release job checks each manifest's exact version on crates.io and skips
an existing version. Otherwise it runs `cargo publish -p beankeeper --locked`,
waits for Cargo to confirm registry availability, then runs
`cargo publish -p beankeeper-cli --locked`. Registry lookup failures stop the job
rather than being treated as missing versions. Published versions are immutable;
confirm an existing library version is the intended dependency before tagging.
Rerun the failed job to retry; do not bump versions or retag solely for a retry.

### Bootstrap the CLI once

`beankeeper-cli` cannot configure a trusted publisher until its first version
exists on crates.io. Until the owner completes setup, the job warns and skips a
CLI upload rejected for authentication or permission reasons. Library failures
and other CLI failures still fail the publishing job. Check the warning before
assuming a successful workflow published both crates.

After merge, a successful release build, and library publication through the job,
the maintainer or PM performs the **first CLI publish once, manually with a
crates.io API token** authorized to create `beankeeper-cli`. Confirm the library's
manifest version is available, authenticate Cargo with that token, and run:

```sh
# Maintainer/PM only: one-time CLI bootstrap after the library is live.
cargo info beankeeper@0.7.0 --registry crates-io
cargo publish -p beankeeper-cli --dry-run --locked
cargo publish -p beankeeper-cli --locked
```

Reread the manifests at release time; the versions here are examples. A CLI-only
dry run needs the matching library in the registry because Cargo removes the
local dependency path when packaging.

The owner then opens `beankeeper-cli` settings on crates.io and adds the same
trusted publisher: GitHub repository `Govcraft/beankeeper`, workflow `release.yml`.
Enable the requirement for Trusted Publishing for all new versions, as on the
library. Subsequent CLI releases use the job's OIDC credential. Rerunning the job
after bootstrap skips both versions that are already live.

After both crates are published, verify a source install in a temporary root:

```sh
install_dir=$(mktemp -d)
cargo install beankeeper-cli --version 0.25.0 --locked --root "$install_dir"
"$install_dir/bin/bk" --version
```

Remove that temporary root afterward. For a binary install, first complete the
GitHub CLI release procedure above, then use `cargo binstall beankeeper-cli
--version 0.25.0`. The package name is `beankeeper-cli`; it installs the executable
`bk`. Its manifest's binstall templates point at
`beankeeper-cli-vX.Y.Z/bk-X.Y.Z-TARGET.tar.gz`, with `bk` inside the
`bk-X.Y.Z-TARGET/` directory. These names match the four Linux GNU and macOS
targets in `release.yml`. Versions without matching GitHub assets cannot use
this download path; the source installation remains available after publishing.
