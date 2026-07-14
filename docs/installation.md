# Pummel Installation and Distribution

Pummel releases are distributed from GitHub Releases with a signed SHA256
manifest. The trust root is the committed `docs/release-signing.pub` public key
plus a locally installed `minisign` binary that you trust.

The library is also published to [crates.io](https://crates.io/crates/pummel).

## 1. Supported Platform Matrix

| Operating System | Architecture | Target | Package Format |
| ---------------- | ------------ | ------ | -------------- |
| **Linux** | Intel/AMD (x86_64) | `x86_64-unknown-linux-gnu` | `tar.gz` |
| **macOS** | Intel (x86_64) | `x86_64-apple-darwin` | `tar.gz` |
| **macOS** | Apple Silicon | `aarch64-apple-darwin` | `tar.gz` |
| **Windows** | Intel/AMD (x86_64) | `x86_64-pc-windows-msvc` | `zip` |

## 2. Prerequisites

Automated installers deliberately **do not** download `minisign` for you.
Install `minisign` from a trusted package manager or release source before
running an installer:

```bash
# macOS
brew install minisign

# Debian/Ubuntu
sudo apt-get install minisign

# Fedora
sudo dnf install minisign

# Arch
sudo pacman -S minisign
```

On Windows, install `minisign` with a trusted package manager such as `winget`,
Chocolatey, or Scoop, then ensure `minisign.exe` is available in `PATH`.

## 3. Automated Installers

The installers:

1. Detect the supported OS and architecture before network downloads.
2. Discover the latest stable GitHub Release tag matching `vMAJOR.MINOR.PATCH`,
   skipping prereleases.
3. Download `checksums-sha256.txt` and `checksums-sha256.txt.minisig`.
4. Verify the manifest with `minisign` and the embedded Pummel public key.
5. Download exactly one platform archive and compare its SHA256 hash against the
   exact filename entry in the verified manifest.
6. Extract only the expected binary member and fail closed on path traversal or
   unexpected archive contents.

### macOS and Linux

```bash
curl -fsSL https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.sh | bash
```

Optional overrides:

```bash
PUMMEL_VERSION=0.1.0 bash scripts/install.sh   # normalized to v0.1.0
PUMMEL_INSTALL_DIR="$HOME/bin" bash scripts/install.sh
```

### Windows PowerShell

```powershell
irm https://raw.githubusercontent.com/OrekGames/pummel/main/scripts/install.ps1 | iex
```

Optional overrides:

```powershell
$env:PUMMEL_VERSION = "0.1.0"
$env:PUMMEL_INSTALL_DIR = "$HOME\.local\bin"
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

Download the public key, signed manifest, detached signature, and archive:

```bash
curl --fail --show-error --location -o release-signing.pub \
  https://raw.githubusercontent.com/OrekGames/pummel/main/docs/release-signing.pub
curl --fail --show-error --location -O "${BASE_URL}/checksums-sha256.txt"
curl --fail --show-error --location -O "${BASE_URL}/checksums-sha256.txt.minisig"
curl --fail --show-error --location -O "${BASE_URL}/${ARCHIVE}"
```

Verify the manifest signature:

```bash
minisign -V -p release-signing.pub -m checksums-sha256.txt -x checksums-sha256.txt.minisig
```

Verify the exact archive hash from the trusted manifest:

```bash
EXPECTED_HASH="$(awk -v name="${ARCHIVE}" '$2 == name { print $1; found=1 } END { if (!found) exit 1 }' checksums-sha256.txt)"
ACTUAL_HASH="$(shasum -a 256 "${ARCHIVE}" | awk '{ print $1 }')"
test "${EXPECTED_HASH}" = "${ACTUAL_HASH}"
```

Extract and install only after both verification steps succeed. Prefer extracting
a single member:

```bash
tar -xzf "${ARCHIVE}" pummel
install -m 0755 pummel /usr/local/bin/pummel
```

## 5. Library Installation via crates.io

```toml
[dependencies]
pummel = "0.1.0"
```

Or:

```bash
cargo add pummel
```

## 6. GitHub Release Maintainer Checklist

Before cutting a protected release tag:

- Use canonical tags of the form `vMAJOR.MINOR.PATCH` only.
- Ensure `Cargo.toml` version equals the tag without the leading `v`.
- Store the minisign private key as the protected GitHub Actions secret
  `MINISIGN_SECRET_KEY` in the `release` environment.
- Confirm the CI private key matches `docs/release-signing.pub` before the first
  release; the release workflow verifies the generated signature before upload.
- Keep private keys out of the repository and never print them in logs.
- Treat release asset uploads as immutable; do not overwrite published assets for
  an existing tag.

## 7. crates.io Publishing Checklist

- Confirm the crate name is still available immediately before first publish.
- Run `cargo package --list` and `cargo publish --dry-run` locally.
- For the **first** publish only, set a short-lived `CARGO_REGISTRY_TOKEN` in the
  protected GitHub `release` environment and let `.github/workflows/release.yml`
  publish from the `vX.Y.Z` tag.
- After the first version exists on crates.io, configure Trusted Publishing for
  repository `OrekGames/pummel`, workflow `release.yml`, and environment
  `release`, then revoke the bootstrap token.
- Later releases should authenticate with Trusted Publishing OIDC only.
