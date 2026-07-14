# Pummel Installation and Distribution

Pummel is published as a Rust crate on [crates.io](https://crates.io/crates/pummel)
and as platform binaries on [GitHub Releases](https://github.com/OrekGames/pummel/releases).

**Preferred install for Rust users:** `cargo install pummel`

Binary installers verify GitHub Release assets by matching the archive SHA-256
against the exact filename entry in `checksums-sha256.txt`. They do not require
`minisign` or any separate signing tool.

> **Note:** The first checksum-verified GitHub Release and crates.io publish are
> forthcoming until tag `v0.1.0` exists. Until then, build from source or wait
> for that release.

## 1. Supported Platform Matrix

| Operating System | Architecture | Target | Package Format |
| ---------------- | ------------ | ------ | -------------- |
| **Linux** | Intel/AMD (x86_64) | `x86_64-unknown-linux-gnu` | `tar.gz` |
| **macOS** | Intel (x86_64) | `x86_64-apple-darwin` | `tar.gz` |
| **macOS** | Apple Silicon | `aarch64-apple-darwin` | `tar.gz` |
| **Windows** | Intel/AMD (x86_64) | `x86_64-pc-windows-msvc` | `zip` |

## 2. Install from crates.io (recommended for Rust users)

```bash
cargo install pummel --locked
```

Library dependency:

```toml
[dependencies]
pummel = "0.1.0"
```

## 3. Automated Binary Installers

The installers:

1. Detect the supported OS and architecture before network downloads.
2. Discover the latest stable GitHub Release tag matching `vMAJOR.MINOR.PATCH`,
   skipping prereleases (or use `PUMMEL_VERSION`).
3. Download `checksums-sha256.txt` and exactly one platform archive.
4. Compare the archive SHA-256 against the exact filename entry in the
   checksum manifest (fail closed if the entry is missing or mismatched).
5. Extract only the expected root binary member and refuse path traversal,
   unexpected members, and symlink/special members.

### macOS and Linux

```bash
curl -fsSL https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.sh | bash
```

Optional overrides:

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

Optional overrides:

```powershell
$env:PUMMEL_VERSION = "0.1.0"
$env:PUMMEL_INSTALL_DIR = "$HOME\.local\bin"
$env:PUMMEL_DOWNLOAD_BASE = "https://example.invalid/pummel"  # /${version} appended
.\scripts\install.ps1
```

## 4. Manual Installation and Verification

Set the version and archive name for your platform. Release tags and archive
names always use the `vMAJOR.MINOR.PATCH` form:

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

## 5. Build from Source

```bash
git clone https://github.com/OrekGames/pummel.git
cd pummel
cargo build --release
```

The CLI binary is at `target/release/pummel`.

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
   `cargo install pummel`.

Subsequent releases should rely on Trusted Publishing only; leave
`FIRST_CRATE_PUBLISH` unset so a failed OIDC login fails closed.
