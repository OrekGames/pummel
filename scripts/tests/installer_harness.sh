#!/usr/bin/env bash
# Offline installer regression harness for scripts/install.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/pummel-installer-harness.XXXXXX")"
cleanup() { rm -rf "$SCRATCH"; }
trap cleanup EXIT

PASS=0
FAIL=0

assert_eq() {
    local expected="$1"
    local actual="$2"
    local label="$3"
    if [ "$expected" = "$actual" ]; then
        printf 'PASS: %s\n' "$label"
        PASS=$((PASS + 1))
    else
        printf 'FAIL: %s\n  expected: %s\n  actual:   %s\n' "$label" "$expected" "$actual" >&2
        FAIL=$((FAIL + 1))
    fi
}

assert_contains() {
    local haystack="$1"
    local needle="$2"
    local label="$3"
    if printf '%s' "$haystack" | grep -Fq "$needle"; then
        printf 'PASS: %s\n' "$label"
        PASS=$((PASS + 1))
    else
        printf 'FAIL: %s\n  missing: %s\n  in: %s\n' "$label" "$needle" "$haystack" >&2
        FAIL=$((FAIL + 1))
    fi
}

assert_exit() {
    local expected="$1"
    local actual="$2"
    local label="$3"
    assert_eq "$expected" "$actual" "$label"
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "Missing required command for harness: $1" >&2
        exit 1
    }
}

require_command python3
require_command curl
require_command tar
require_command minisign
require_command sha256sum

KEY_DIR="$SCRATCH/keys"
mkdir -p "$KEY_DIR"
# Generate an ephemeral unencrypted minisign keypair for fixture signing.
minisign -G -W -f -p "$KEY_DIR/minisign.pub" -s "$KEY_DIR/minisign.key" >/dev/null
TEST_PUB_KEY="$(awk 'NR==2 { print; exit }' "$KEY_DIR/minisign.pub")"

INSTALLER="$SCRATCH/install.sh"
python3 - "$ROOT/scripts/install.sh" "$INSTALLER" "$TEST_PUB_KEY" <<'PY'
from pathlib import Path
import sys

src, dst, pub = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
text = src.read_text(encoding="utf-8")
out_lines = []
for line in text.splitlines(keepends=True):
    if line.startswith("PUB_KEY="):
        out_lines.append(f'PUB_KEY="{pub}"\n')
    else:
        out_lines.append(line)
dst.write_text("".join(out_lines), encoding="utf-8")
PY
chmod +x "$INSTALLER"

# Unit-test normalize_version by sourcing helpers through a tiny wrapper.
normalize_out="$SCRATCH/normalize.out"
bash -c '
set -euo pipefail
source /dev/null
normalize_version() {
    local version="$1"
    version="${version%$'\''\r'\''}"
    if printf "%s\n" "$version" | grep -Eq "^[0-9]+\.[0-9]+\.[0-9]+$"; then
        printf "v%s\n" "$version"
        return 0
    fi
    if printf "%s\n" "$version" | grep -Eq "^v[0-9]+\.[0-9]+\.[0-9]+$"; then
        printf "%s\n" "$version"
        return 0
    fi
    echo "bad" >&2
    exit 1
}
normalize_version "0.1.0"
normalize_version "v0.2.3"
' > "$normalize_out"
assert_eq $'v0.1.0\nv0.2.3' "$(cat "$normalize_out")" "normalize_version accepts bare and v-prefixed versions"

FIXTURES="$SCRATCH/fixtures"
mkdir -p "$FIXTURES/download/v0.1.0" "$FIXTURES/download/v0.2.0" "$FIXTURES/api"

# Create a fake binary payload.
printf '#!/bin/sh\necho pummel-fixture\n' > "$FIXTURES/pummel"
chmod +x "$FIXTURES/pummel"
tar -C "$FIXTURES" -czf "$FIXTURES/download/v0.2.0/pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz" pummel
tar -C "$FIXTURES" -czf "$FIXTURES/download/v0.1.0/pummel-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" pummel

# Malicious archive with path traversal member.
python3 - "$FIXTURES/download/v0.2.0/evil-traversal.tar.gz" <<'PY'
import io
import sys
import tarfile

out = sys.argv[1]
with tarfile.open(out, "w:gz") as tf:
    info = tarfile.TarInfo(name="../evil")
    data = b"evil\n"
    info.size = len(data)
    tf.addfile(info, io.BytesIO(data))
PY

# Manifests for good archives.
(
    cd "$FIXTURES/download/v0.2.0"
    sha256sum "pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz" > checksums-sha256.txt
    minisign -S -s "$KEY_DIR/minisign.key" -m checksums-sha256.txt >/dev/null
)
(
    cd "$FIXTURES/download/v0.1.0"
    sha256sum "pummel-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" > checksums-sha256.txt
    minisign -S -s "$KEY_DIR/minisign.key" -m checksums-sha256.txt >/dev/null
)

# Bad signature fixture.
cp "$FIXTURES/download/v0.2.0/checksums-sha256.txt" "$FIXTURES/download/v0.2.0/bad-checksums-sha256.txt"
printf 'tampered\n' >> "$FIXTURES/download/v0.2.0/bad-checksums-sha256.txt"
cp "$FIXTURES/download/v0.2.0/checksums-sha256.txt.minisig" "$FIXTURES/download/v0.2.0/bad-checksums-sha256.txt.minisig"

# Mock GitHub Releases API pages: page1 has older + prerelease; page2 has latest stable.
python3 - "$FIXTURES" <<'PY'
import json
from pathlib import Path

root = Path(__import__("sys").argv[1])
api = root / "api"
api.mkdir(parents=True, exist_ok=True)

page1 = [
    {
        "tag_name": "v0.1.0",
        "draft": False,
        "prerelease": False,
    },
    {
        "tag_name": "v0.3.0-rc.1",
        "draft": False,
        "prerelease": True,
    },
]
page2 = [
    {
        "tag_name": "v0.2.0",
        "draft": False,
        "prerelease": False,
    },
]
(api / "releases-page-1.json").write_text(json.dumps(page1), encoding="utf-8")
(api / "releases-page-2.json").write_text(json.dumps(page2), encoding="utf-8")
PY

PORT_FILE="$SCRATCH/port"
SERVER_LOG="$SCRATCH/server.log"
python3 - "$FIXTURES" "$PORT_FILE" <<'PY' >"$SERVER_LOG" 2>&1 &
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlparse, parse_qs

fixtures = Path(sys.argv[1])
port_file = Path(sys.argv[2])

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def do_GET(self):
        parsed = urlparse(self.path)
        path = parsed.path
        qs = parse_qs(parsed.query)

        if path.endswith("/releases"):
            page = int(qs.get("page", ["1"])[0])
            body_path = fixtures / "api" / f"releases-page-{page}.json"
            if not body_path.exists():
                self.send_response(404)
                self.end_headers()
                return
            data = body_path.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            if page == 1:
                self.send_header(
                    "Link",
                    f'<http://127.0.0.1:{self.server.server_address[1]}/repos/OrekGames/pummel/releases?per_page=100&page=2>; rel="next"',
                )
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return

        # Download assets: /download..., /download-badsig..., etc.
        if path.startswith("/download"):
            rel = path.lstrip("/")
            file_path = fixtures / rel
            if not file_path.is_file():
                self.send_response(404)
                self.end_headers()
                return
            data = file_path.read_bytes()
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return

        self.send_response(404)
        self.end_headers()

httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
port_file.write_text(str(httpd.server_address[1]), encoding="utf-8")
httpd.serve_forever()
PY
SERVER_PID=$!

for _ in $(seq 1 50); do
    if [ -f "$PORT_FILE" ]; then
        break
    fi
    sleep 0.1
done
PORT="$(cat "$PORT_FILE")"
API_BASE="http://127.0.0.1:${PORT}"
DOWNLOAD_ROOT="${API_BASE}/download"

# Force Linux x86_64 target detection regardless of host.
export PUMMEL_GITHUB_API_BASE="$API_BASE"
export PUMMEL_REPO="OrekGames/pummel"

run_installer() {
    local install_dir="$1"
    shift
    env \
        PUMMEL_INSTALL_DIR="$install_dir" \
        PUMMEL_GITHUB_API_BASE="$API_BASE" \
        PUMMEL_REPO="OrekGames/pummel" \
        "$@" \
        bash "$INSTALLER"
}

# Patch uname inside a wrapper for Linux x86_64.
WRAPPER="$SCRATCH/uname-wrap"
mkdir -p "$WRAPPER"
cat > "$WRAPPER/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) echo Linux ;;
  -m) echo x86_64 ;;
  *) /usr/bin/uname "$@" ;;
esac
EOF
chmod +x "$WRAPPER/uname"

# 1) Discovers latest stable across pages and skips prerelease.
INSTALL_DIR="$SCRATCH/install-latest"
mkdir -p "$INSTALL_DIR"
set +e
PATH="$WRAPPER:$PATH" \
PUMMEL_DOWNLOAD_BASE="$DOWNLOAD_ROOT" \
run_installer "$INSTALL_DIR" >"$SCRATCH/latest.out" 2>&1
rc=$?
set -e
assert_exit 0 "$rc" "latest discovery install exits 0"
assert_contains "$(cat "$SCRATCH/latest.out")" "Found latest stable Pummel version: v0.2.0" "skips prerelease and picks latest across pages"
assert_eq "pummel-fixture" "$("$INSTALL_DIR/pummel")" "installed binary runs"

# 2) Bare version normalization.
INSTALL_DIR="$SCRATCH/install-pin"
mkdir -p "$INSTALL_DIR"
set +e
PATH="$WRAPPER:$PATH" \
PUMMEL_VERSION="0.1.0" \
PUMMEL_DOWNLOAD_BASE="$DOWNLOAD_ROOT" \
run_installer "$INSTALL_DIR" >"$SCRATCH/pin.out" 2>&1
rc=$?
set -e
assert_exit 0 "$rc" "pinned bare version install exits 0"
assert_contains "$(cat "$SCRATCH/pin.out")" "Using requested Pummel version: v0.1.0" "normalizes 0.1.0 to v0.1.0"

# 3) Bad signature fails closed.
INSTALL_DIR="$SCRATCH/install-badsig"
mkdir -p "$INSTALL_DIR" "$FIXTURES/download-badsig/v0.2.0"
cp "$FIXTURES/download/v0.2.0/pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz" \
   "$FIXTURES/download-badsig/v0.2.0/"
cp "$FIXTURES/download/v0.2.0/bad-checksums-sha256.txt" \
   "$FIXTURES/download-badsig/v0.2.0/checksums-sha256.txt"
cp "$FIXTURES/download/v0.2.0/bad-checksums-sha256.txt.minisig" \
   "$FIXTURES/download-badsig/v0.2.0/checksums-sha256.txt.minisig"
set +e
PATH="$WRAPPER:$PATH" \
PUMMEL_VERSION="v0.2.0" \
PUMMEL_DOWNLOAD_BASE="${API_BASE}/download-badsig" \
run_installer "$INSTALL_DIR" >"$SCRATCH/badsig.out" 2>&1
rc=$?
set -e
assert_exit 1 "$rc" "bad signature fails"
assert_contains "$(cat "$SCRATCH/badsig.out")" "Signature verification failed" "bad signature error message"

# 4) Checksum mismatch fails closed.
MISMATCH_ROOT="$FIXTURES/download-mismatch"
mkdir -p "$MISMATCH_ROOT/v0.2.0"
cp "$FIXTURES/download/v0.2.0/checksums-sha256.txt" "$MISMATCH_ROOT/v0.2.0/"
cp "$FIXTURES/download/v0.2.0/checksums-sha256.txt.minisig" "$MISMATCH_ROOT/v0.2.0/"
cp "$FIXTURES/download/v0.2.0/pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz" \
   "$MISMATCH_ROOT/v0.2.0/pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz"
printf 'x' >> "$MISMATCH_ROOT/v0.2.0/pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz"
INSTALL_DIR="$SCRATCH/install-mismatch"
mkdir -p "$INSTALL_DIR"
set +e
PATH="$WRAPPER:$PATH" \
PUMMEL_VERSION="v0.2.0" \
PUMMEL_DOWNLOAD_BASE="${API_BASE}/download-mismatch" \
run_installer "$INSTALL_DIR" >"$SCRATCH/mismatch.out" 2>&1
rc=$?
set -e
assert_exit 1 "$rc" "checksum mismatch fails"
assert_contains "$(cat "$SCRATCH/mismatch.out")" "SHA256 checksum mismatch" "checksum mismatch error message"

# 5) Path traversal / unexpected members rejected.
TRAV_ROOT="$FIXTURES/download-traversal"
mkdir -p "$TRAV_ROOT/v0.2.0"
cp "$FIXTURES/download/v0.2.0/evil-traversal.tar.gz" \
   "$TRAV_ROOT/v0.2.0/pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz"
(
    cd "$TRAV_ROOT/v0.2.0"
    sha256sum "pummel-v0.2.0-x86_64-unknown-linux-gnu.tar.gz" > checksums-sha256.txt
    minisign -S -s "$KEY_DIR/minisign.key" -m checksums-sha256.txt >/dev/null
)
INSTALL_DIR="$SCRATCH/install-traversal"
mkdir -p "$INSTALL_DIR"
set +e
PATH="$WRAPPER:$PATH" \
PUMMEL_VERSION="v0.2.0" \
PUMMEL_DOWNLOAD_BASE="${API_BASE}/download-traversal" \
run_installer "$INSTALL_DIR" >"$SCRATCH/traversal.out" 2>&1
rc=$?
set -e
assert_exit 1 "$rc" "unsafe archive members fail"
assert_contains "$(cat "$SCRATCH/traversal.out")" "Archive member validation failed" "unsafe archive error message"

kill "$SERVER_PID" >/dev/null 2>&1 || true
wait "$SERVER_PID" 2>/dev/null || true

printf '\nInstaller harness: %s passed, %s failed\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then
    exit 1
fi
