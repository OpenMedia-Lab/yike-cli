# Releasing Yike CLI

The public repository starts from a reviewed source snapshot with a new Git root. Never merge the private working repository or its history into this repository. Keep runtime credentials, account catalogs, transfer databases and media outside the public checkout. A release is built from one existing Git tag; the workflow never creates or moves that tag.

## Native build contract

Both branch/PR CI and the release workflow use the same reusable workflow and Rust **1.98.1**, with the committed `Cargo.lock`. Every row runs formatting, Clippy with warnings denied, all applicable anonymous Rust tests, Python packaging regressions, a native release build and the built executable's `--version`, `--help` and anonymous `doctor --offline` smoke check.

| Download target | Native GitHub runner | Archive |
| --- | --- | --- |
| Windows x64 | `windows-2025` | `yike-VERSION-x86_64-pc-windows-msvc.zip` |
| Linux / WSL x64 | `ubuntu-24.04` | `yike-VERSION-x86_64-unknown-linux-gnu.tar.gz` |
| Linux / WSL ARM64 | `ubuntu-24.04-arm` | `yike-VERSION-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `macos-15` | `yike-VERSION-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `macos-15-intel` | `yike-VERSION-x86_64-apple-darwin.tar.gz` |

These explicit labels and architectures are listed in [GitHub's hosted runner reference](https://docs.github.com/en/actions/reference/runners/github-hosted-runners). The [Windows image](https://github.com/actions/runner-images/blob/main/images/windows/Windows2025-Readme.md) includes Visual Studio C++ tools and a Windows SDK. Its job locates the x64 MSVC linker before compiling and testing. A Rust toolchain installed on a laptop without `link.exe` cannot substitute for a successful native Windows job.

The current Unix suite contains 84 Rust tests. Platform-specific tests are selected by Rust's `cfg` attributes, so counts can differ by operating system. Tests use synthetic data and loopback fixtures; CI receives no production API credentials. This verifies local program behavior and packaging. It does not establish live service compatibility, sign the executables or notarize the macOS builds. Linux binaries require a compatible GNU/Linux userspace, including when used under WSL; they are not static universal Linux executables.

## Prepare and trigger

1. Review the public diff, including `Cargo.toml`, `Cargo.lock`, release workflows and generated archive contents. Check that the source contains only anonymous fixtures and public project files.
2. Set `package.version` in `Cargo.toml` to the intended stable `MAJOR.MINOR.PATCH` version, update the lockfile when appropriate, and merge the reviewed changes. Let every native branch CI job pass.
3. Tag that reviewed public commit, using exactly `v` followed by `package.version` (for example `v0.1.0`), and push that tag to the public repository. Do not tag the private repository's history. The tag push triggers `Release` automatically; there is no publishing path for pull requests.
4. Follow the `Release` run. Every native test and build must pass before its publishing job can start. The Windows MSVC job must actually pass; a local SDK problem is not a reason to skip it.

Reviewers can reproduce the Rust checks with:

```sh
cargo +1.98.1 fmt --check
cargo +1.98.1 clippy --locked --all-targets -- -D warnings
cargo +1.98.1 test --locked --all-targets
cargo +1.98.1 build --locked --release
```

Use Python 3.11 or later for the release helper. These checks do not build Rust or contact an account:

```sh
python3 -B -m unittest discover -s tests -p 'test_*.py'
python3 -B scripts/package-release.py version --tag v0.1.0
```

The packaging tests use synthetic executable bytes. To check an already built native executable without recompiling:

```sh
python3 -B scripts/package-release.py smoke --binary target/release/yike
python3 -B scripts/package-release.py package \
  --tag v0.1.0 --target aarch64-apple-darwin \
  --binary target/release/yike --out-dir /tmp/yike-release-check
```

Select the target matching the executable. On Windows use `python`, `target\release\yike.exe` and target `x86_64-pc-windows-msvc`. A package contains exactly three regular files under its versioned target directory: `yike` (or `yike.exe`), `LICENSE`, and a generated `README.txt`. The helper copies no directories, project README, runtime state or media. Archive metadata is deterministic for identical inputs; this is not a claim of reproducible compiler output across runner image updates.

## Publication and recovery

`Release` validates the tag against Cargo's version and records the checked-out source commit. It builds all five targets from that frozen workflow revision. A failed matrix job prevents publication. Build handoff artifacts expire after three days; the published archives are ordinary [GitHub Release assets](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases), independent of that artifact retention window.

Only the final publishing job has `contents: write`. It uses the built-in `GITHUB_TOKEN` through [GitHub CLI](https://cli.github.com/manual/gh_release_create); no personal access token or third-party release action is needed. GitHub-owned checkout and artifact actions are pinned to reviewed full commit hashes. Review those pins and runner labels when updating the workflow.

The publishing job requires exactly the five expected archive filenames and computes `SHA256SUMS`. It then creates an unpublished draft, uploads the six assets, downloads the staged files again, checks their names and hashes, confirms the tag still points at the recorded commit and publishes the draft as its final step. The release notes identify the source commit. Tag runs are serialized, and an existing published release is never overwritten by a rerun.

If tests or builds fail, fix the source and issue a new version/tag. If upload or verification fails after draft creation, the release remains a draft. Inspect the failed job, then rerun it while its handoff artifacts are available; a matching draft can be resumed with replacement assets. Unexpected draft assets or a source commit mismatch stop publication and require maintainer review. After artifacts expire, rerun all jobs to rebuild the complete set. Do not manually publish a failed draft or move a released tag.

After downloading a release, verify the selected archive against `SHA256SUMS` before extraction:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

On macOS use `shasum -a 256 ARCHIVE_NAME` and compare the full digest with its line in `SHA256SUMS`. On Windows use `Get-FileHash -Algorithm SHA256 .\ARCHIVE_NAME` in PowerShell and compare the full digest. At least the selected archive must be present and match; do not treat a missing-file message as a successful verification.
