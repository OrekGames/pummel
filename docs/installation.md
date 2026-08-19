# Pummel Installation and Distribution

> **Pre-release status:** tag `v0.1.0` has not been pushed. Pummel is **not**
> published on [crates.io](https://crates.io/crates/pummel) and there are **no**
> [GitHub Releases](https://github.com/OrekGames/pummel/releases). Until that
> tag exists, use [build from source](#2-build-from-source-works-today) or
> `cargo install --git`. Do not start with `cargo install pummel` or the curl /
> `irm` installers; those paths fail today.

Binary installers (documented below for the first release) verify GitHub
Release assets by matching the archive SHA-256 against the exact filename
entry in `checksums-sha256.txt`. They do not require `minisign` or any
separate signing tool.

## 1. Supported Platform Matrix

| Operating System | Architecture | Target | Package Format |
| ---------------- | ------------ | ------ | -------------- |
| **Linux** | Intel/AMD (x86_64) | `x86_64-unknown-linux-gnu` | `tar.gz` |
| **macOS** | Intel (x86_64) | `x86_64-apple-darwin` | `tar.gz` |
| **macOS** | Apple Silicon | `aarch64-apple-darwin` | `tar.gz` |
| **Windows** | Intel/AMD (x86_64) | `x86_64-pc-windows-msvc` | `zip` |

## 2. Build from Source (works today)

This is the supported install path until `v0.1.0`.

```bash
git clone https://github.com/OrekGames/pummel.git
cd pummel
cargo build --release
```

The CLI binary is `target/release/pummel`. README examples use that path.

### Install the CLI from git

```bash
cargo install --git https://github.com/OrekGames/pummel.git --locked pummel
```

This puts `pummel` on your Cargo bin directory (`~/.cargo/bin` by default).
Confirm that directory is on `PATH`.

### Library dependency (git)

```toml
[dependencies]
pummel = { git = "https://github.com/OrekGames/pummel.git" }
```

### Docker (optional)

```bash
docker build -f docker/Dockerfile -t pummel .
docker run --rm pummel --help
```

The image entrypoint is the `pummel` CLI. Example configs are copied to
`/examples/` in the image.

## 3. Install from crates.io (after `v0.1.0`)

When the crate is published:

```bash
cargo install pummel --locked
```

Library dependency:

```toml
[dependencies]
pummel = "0.1.0"
```

Until then, `cargo install pummel` fails because the crate name is not on
crates.io.

## 4. Automated Binary Installers (after `v0.1.0`)

The installers:

1. Detect the supported OS and architecture before network downloads.
2. Discover the latest stable GitHub Release tag matching `vMAJOR.MINOR.PATCH`,
   skipping prereleases (or use `PUMMEL_VERSION`).
3. Download `checksums-sha256.txt` and exactly one platform archive.
4. Compare the archive SHA-256 against the exact filename entry in the
   checksum manifest (fail closed if the entry is missing or mismatched).
5. Extract only the expected root binary member and refuse path traversal,
   unexpected members, and symlink/special members.

**Today:** with no stable releases, a default installer run fails closed with
`No stable vMAJOR.MINOR.PATCH releases found on GitHub`. Do not use these
one-liners until tag `v0.1.0` exists. Pinning `PUMMEL_VERSION=0.1.0` also
fails until that release is published.

### macOS and Linux

```bash
curl -fsSL https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.sh | bash
```

The bash installer installs to `~/.local/bin` by default. Add that directory
to `PATH` if `pummel` is not found after a successful install.

Optional overrides (only useful once a matching release exists):

```bash
PUMMEL_VERSION=0.1.0 bash scripts/install.sh   # normalized to v0.1.0
PUMMEL_INSTALL_DIR="$HOME/bin" bash scripts/install.sh
# Root URL; /${version} is appended automatically:
PUMMEL_DOWNLOAD_BASE="https://example.invalid/pummel" bash scripts/install.sh
```

### Windows PowerShell

```powershell
irm https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.ps1 | iex
```

Optional overrides (only useful once a matching release exists):

```powershell
$env:PUMMEL_VERSION = "0.1.0"
$env:PUMMEL_INSTALL_DIR = "$HOME\.local\bin"
$env:PUMMEL_DOWNLOAD_BASE = "https://example.invalid/pummel"  # /${version} appended
.\scripts\install.ps1
```

## 5. Manual Installation and Verification (after `v0.1.0`)

Set the version and archive name for your platform. Release tags and archive
names always use the `vMAJOR.MINOR.PATCH` form. These URLs 404 until the
GitHub Release exists.

```bash
VERSION="v0.1.0"
ARCHIVE="pummel-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
BASE_URL="https://github.com/OrekGames/pummel/releases/download/${VERSION}"
```

Download the checksum manifest and archive:

```bash
curl --fail --show-error --location -O "${BASE_URL}/checksums-sha256.txt"
curl --fail --show-error --location -O "${BASE_URL}/${ARCHIVE}"
```

Verify the exact archive hash from the manifest:

```bash
EXPECTED_HASH="$(awk -v name="${ARCHIVE}" '$2 == name { print tolower($1); found=1 } END { if (!found) exit 1 }' checksums-sha256.txt)"
ACTUAL_HASH="$(shasum -a 256 "${ARCHIVE}" | awk '{ print tolower($1) }')"
test "${EXPECTED_HASH}" = "${ACTUAL_HASH}"
```

Extract and install only after checksum verification succeeds. Prefer extracting
a single member:

```bash
tar -xzf "${ARCHIVE}" pummel
install -m 755 pummel /usr/local/bin/pummel
```

On Windows, expand the zip and confirm it contains only a root-level
`pummel.exe` before copying it onto your `PATH`.

## 6. Maintainer Release Checklist

For the first public release (`v0.1.0`):

1. Set temporary `CARGO_REGISTRY_TOKEN` in the GitHub Actions `release`
   environment and set environment variable `FIRST_CRATE_PUBLISH=true`
   (bootstrap only).
2. Tag `v0.1.0` (must match `Cargo.toml` version) and push the tag.
3. Confirm the Release workflow publishes crates.io first, then creates the
   GitHub Release with four platform archives plus `checksums-sha256.txt`.
4. Configure crates.io Trusted Publishing for this repository, then revoke the
   bootstrap token and clear `FIRST_CRATE_PUBLISH`.
5. Smoke-test `scripts/install.sh`, `scripts/install.ps1`, and
   `cargo install pummel --locked`.
6. Rewrite the pre-release banners in this file and the README so crates.io
   and the installers are the preferred paths.

Subsequent releases should rely on Trusted Publishing only; leave
`FIRST_CRATE_PUBLISH` unset so a failed OIDC login fails closed.
