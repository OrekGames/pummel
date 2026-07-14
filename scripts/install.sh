#!/usr/bin/env bash

# Pummel - Installer for macOS and Linux
# Discovers the latest stable GitHub Release, verifies the archive SHA256
# checksum against checksums-sha256.txt, and installs the platform binary.

set -euo pipefail

GITHUB_API_BASE="${PUMMEL_GITHUB_API_BASE:-https://api.github.com}"
GITHUB_API_BASE="${GITHUB_API_BASE%/}"
GITHUB_REPO="${PUMMEL_REPO:-OrekGames/pummel}"
REQUESTED_VERSION="${PUMMEL_VERSION:-}"
INSTALL_DIR_OVERRIDE="${PUMMEL_INSTALL_DIR:-}"

OS=""
ARCH=""
TARGET=""
ARCHIVE_EXT=""
SCRATCH_DIR=""
SHA256_TOOL=""

info() {
    printf '==> %s\n' "$*"
}

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "${SCRATCH_DIR:-}" ] && [ -d "$SCRATCH_DIR" ]; then
        rm -rf "$SCRATCH_DIR"
    fi
}

on_interrupt() {
    cleanup
    exit 130
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

detect_target() {
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m)"

    case "$OS" in
        linux)
            case "$ARCH" in
                x86_64)
                    TARGET="x86_64-unknown-linux-gnu"
                    ARCHIVE_EXT="tar.gz"
                    ;;
                *)
                    fail "Unsupported Linux architecture '$ARCH'. Only x86_64 is currently supported."
                    ;;
            esac
            ;;
        darwin)
            case "$ARCH" in
                x86_64)
                    TARGET="x86_64-apple-darwin"
                    ARCHIVE_EXT="tar.gz"
                    ;;
                arm64|aarch64)
                    TARGET="aarch64-apple-darwin"
                    ARCHIVE_EXT="tar.gz"
                    ;;
                *)
                    fail "Unsupported macOS architecture '$ARCH'. Only x86_64 and arm64/aarch64 are currently supported."
                    ;;
            esac
            ;;
        *)
            fail "Unsupported operating system '$OS'. Only Linux and macOS are currently supported by this installer."
            ;;
    esac
}

select_sha256_tool() {
    if command -v sha256sum >/dev/null 2>&1; then
        SHA256_TOOL="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        SHA256_TOOL="shasum"
    elif command -v openssl >/dev/null 2>&1; then
        SHA256_TOOL="openssl"
    else
        fail "Missing SHA256 tool: install sha256sum, shasum, or openssl"
    fi
}

require_prerequisites() {
    require_command curl
    require_command tar
    require_command awk
    require_command sed
    require_command grep
    require_command sort
    require_command python3
    select_sha256_tool
}

normalize_version() {
    local version="$1"
    version="${version%$'\r'}"
    if printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
        printf 'v%s\n' "$version"
        return 0
    fi
    if printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
        printf '%s\n' "$version"
        return 0
    fi
    fail "Unsupported version '$version'. Expected MAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH"
}

curl_download() {
    local url="$1"
    local output="$2"

    curl --fail --show-error --silent --location \
        --retry 3 --retry-delay 2 \
        --header "Accept: application/vnd.github+json" \
        --header "X-GitHub-Api-Version: 2022-11-28" \
        --output "$output" \
        "$url"
}

discover_latest_version() {
    local page=1
    local body headers next_url versions_file latest

    versions_file="$SCRATCH_DIR/release-versions.txt"
    : > "$versions_file"
    next_url="${GITHUB_API_BASE}/repos/${GITHUB_REPO}/releases?per_page=100&page=1"

    while [ -n "$next_url" ]; do
        body="$SCRATCH_DIR/releases-page-${page}.json"
        headers="$SCRATCH_DIR/releases-page-${page}.headers"
        if ! curl --fail --show-error --silent --location \
            --retry 3 --retry-delay 2 \
            --header "Accept: application/vnd.github+json" \
            --header "X-GitHub-Api-Version: 2022-11-28" \
            --dump-header "$headers" \
            --output "$body" \
            "$next_url"; then
            fail "Failed to fetch GitHub Releases API page $page"
        fi

        python3 - "$body" >> "$versions_file" <<'PY'
import json
import re
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as fh:
    releases = json.load(fh)

pattern = re.compile(r"^v([0-9]+)\.([0-9]+)\.([0-9]+)$")
for release in releases:
    if release.get("draft") or release.get("prerelease"):
        continue
    tag = release.get("tag_name") or ""
    match = pattern.fullmatch(tag)
    if not match:
        continue
    major, minor, patch = (int(part) for part in match.groups())
    print(f"{major} {minor} {patch} {tag}")
PY

        next_url="$(python3 - "$headers" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8", errors="replace").read()
match = re.search(r"(?im)^link:\s*(.*)$", text)
if not match:
    raise SystemExit(0)
for part in match.group(1).split(","):
    part = part.strip()
    m = re.match(r'<([^>]+)>\s*;\s*rel="next"', part)
    if m:
        print(m.group(1))
        break
PY
)"
        if [ -n "$next_url" ]; then
            page=$((page + 1))
        fi
    done

    latest="$(LC_ALL=C sort -k1,1n -k2,2n -k3,3n "$versions_file" | awk 'END { print $4 }')"
    [ -n "$latest" ] || fail "No stable vMAJOR.MINOR.PATCH releases found on GitHub"
    printf '%s\n' "$latest"
}

compute_sha256() {
    local file_path="$1"

    case "$SHA256_TOOL" in
        sha256sum)
            sha256sum "$file_path" | awk '{ print tolower($1) }'
            ;;
        shasum)
            shasum -a 256 "$file_path" | awk '{ print tolower($1) }'
            ;;
        openssl)
            openssl dgst -sha256 "$file_path" | awk '{ print tolower($NF) }'
            ;;
        *)
            fail "Internal error: no SHA256 tool selected"
            ;;
    esac
}

verify_checksum() {
    local manifest_path="$1"
    local archive_name="$2"
    local archive_path="$3"
    local expected_hash actual_hash

    expected_hash="$(awk -v name="$archive_name" '$2 == name { print tolower($1); found=1 } END { if (!found) exit 1 }' "$manifest_path")" || \
        fail "Archive $archive_name not found in checksums-sha256.txt"
    actual_hash="$(compute_sha256 "$archive_path")"

    if [ "$expected_hash" != "$actual_hash" ]; then
        printf 'Expected: %s\nActual:   %s\n' "$expected_hash" "$actual_hash" >&2
        fail "SHA256 checksum mismatch for $archive_name"
    fi
}

install_binary() {
    local archive_path="$1"
    local extract_dir binary_path install_dir resolved extract_dir_resolved

    info "Extracting Pummel binary"
    extract_dir="$SCRATCH_DIR/extract"
    mkdir -p "$extract_dir"

    # Require exactly one regular-file member named pummel (reject symlinks etc.).
    if ! python3 - "$archive_path" <<'PY'
import sys
import tarfile

archive = sys.argv[1]
with tarfile.open(archive, "r:gz") as tf:
    members = tf.getmembers()
    if len(members) != 1:
        print(f"Archive must contain exactly one member; found {len(members)}", file=sys.stderr)
        sys.exit(1)
    member = members[0]
    if member.name != "pummel":
        print(f"Unexpected archive member: {member.name}", file=sys.stderr)
        sys.exit(1)
    if not member.isfile() or member.issym() or member.islnk():
        print(f"Archive member must be a regular file, not {member.type}", file=sys.stderr)
        sys.exit(1)
PY
    then
        fail "Archive member validation failed"
    fi

    tar -xzf "$archive_path" -C "$extract_dir" pummel
    binary_path="$extract_dir/pummel"
    [ -f "$binary_path" ] || fail "Extracted archive did not contain a pummel binary"
    [ ! -L "$binary_path" ] || fail "Refusing to install a symlink binary"
    extract_dir_resolved="$(cd "$extract_dir" && pwd)"
    resolved="$(cd "$(dirname "$binary_path")" && pwd)/$(basename "$binary_path")"
    case "$resolved" in
        "$extract_dir_resolved"/*) ;;
        *) fail "Refusing to install binary outside extract directory: $resolved" ;;
    esac

    if [ -n "$INSTALL_DIR_OVERRIDE" ]; then
        install_dir="$INSTALL_DIR_OVERRIDE"
    elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        install_dir="/usr/local/bin"
    else
        install_dir="$HOME/.local/bin"
        info "/usr/local/bin is not writable; installing to $install_dir"
    fi

    mkdir -p "$install_dir" || fail "Failed to create install directory: $install_dir"
    [ -w "$install_dir" ] || fail "Install directory is not writable: $install_dir"

    cp "$binary_path" "$install_dir/pummel"
    chmod +x "$install_dir/pummel"

    info "Pummel installed successfully to $install_dir/pummel"
    printf 'Run `pummel --version` to verify the installation.\n'
}

main() {
    local version download_base manifest_path archive_name archive_path

    detect_target

    SCRATCH_DIR="$(mktemp -d "${TMPDIR:-/tmp}/pummel-install.XXXXXX")"
    trap cleanup EXIT
    trap on_interrupt INT TERM

    require_prerequisites

    if [ -n "$REQUESTED_VERSION" ]; then
        version="$(normalize_version "$REQUESTED_VERSION")"
        info "Using requested Pummel version: $version"
    else
        info "Discovering latest stable Pummel version from GitHub Releases"
        version="$(discover_latest_version)"
        info "Found latest stable Pummel version: $version"
    fi

    if [ -n "${PUMMEL_DOWNLOAD_BASE:-}" ]; then
        # Override root URL; the selected version directory is appended.
        download_base="${PUMMEL_DOWNLOAD_BASE%/}/${version}"
    else
        download_base="https://github.com/${GITHUB_REPO}/releases/download/${version}"
    fi

    manifest_path="$SCRATCH_DIR/checksums-sha256.txt"
    archive_name="pummel-${version}-${TARGET}.${ARCHIVE_EXT}"
    archive_path="$SCRATCH_DIR/$archive_name"

    info "Downloading checksum manifest"
    curl_download "$download_base/checksums-sha256.txt" "$manifest_path"

    info "Downloading Pummel archive: $archive_name"
    curl_download "$download_base/$archive_name" "$archive_path"

    info "Verifying archive checksum"
    verify_checksum "$manifest_path" "$archive_name" "$archive_path"

    install_binary "$archive_path"
}

main "$@"
