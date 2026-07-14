#!/usr/bin/env bash
# Offline installer harness: exercises checksum verification and archive safety
# without contacting GitHub or requiring minisign.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
INSTALLER="$ROOT/scripts/install.sh"
FIXTURE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pummel-installer-harness.XXXXXX")"
PASS=0
FAIL=0
HTTP_PID=""

cleanup() {
  if [ -n "${HTTP_PID:-}" ]; then
    kill "$HTTP_PID" 2>/dev/null || true
    wait "$HTTP_PID" 2>/dev/null || true
    HTTP_PID=""
  fi
  rm -rf "$FIXTURE_ROOT"
}
trap cleanup EXIT

info() {
  printf '==> %s\n' "$*"
}

pass() {
  PASS=$((PASS + 1))
  printf 'PASS: %s\n' "$*"
}

fail() {
  FAIL=$((FAIL + 1))
  printf 'FAIL: %s\n' "$*" >&2
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing required command for harness: $1" >&2
    exit 1
  }
}

require_cmd bash
require_cmd curl
require_cmd tar
require_cmd python3
require_cmd sha256sum
require_cmd awk
require_cmd ln

HOST_OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
HOST_ARCH="$(uname -m)"
case "$HOST_OS-$HOST_ARCH" in
  linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
  darwin-x86_64) TARGET="x86_64-apple-darwin" ;;
  darwin-arm64|darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
  *)
    echo "Unsupported harness host: $HOST_OS/$HOST_ARCH" >&2
    exit 1
    ;;
esac

write_checksums() {
  local dir="$1"
  local archive="$2"
  local hash
  hash="$(sha256sum "$dir/$archive" | awk '{ print $1 }')"
  printf '%s  %s\n' "$hash" "$archive" > "$dir/checksums-sha256.txt"
}

make_good_archive() {
  local dir="$1"
  local version="$2"
  local target="$3"
  local archive="pummel-${version}-${target}.tar.gz"
  mkdir -p "$dir/bin"
  printf '#!/bin/sh\necho pummel-fixture\n' > "$dir/bin/pummel"
  chmod +x "$dir/bin/pummel"
  # Avoid macOS AppleDouble (._*) members in fixture archives.
  COPYFILE_DISABLE=1 tar -C "$dir/bin" -czf "$dir/$archive" pummel
  write_checksums "$dir" "$archive"
}

run_installer() {
  local download_base="$1"
  local install_dir="$2"
  local version="$3"
  local log="$4"
  env \
    PUMMEL_DOWNLOAD_BASE="$download_base" \
    PUMMEL_INSTALL_DIR="$install_dir" \
    PUMMEL_VERSION="$version" \
    bash "$INSTALLER" >"$log" 2>&1
}

start_http() {
  local root="$1"
  local port_file="$2"
  rm -f "$port_file"
  python3 - "$root" "$port_file" <<'PY' &
import http.server
import os
import socketserver
import sys

root, port_file = sys.argv[1], sys.argv[2]
os.chdir(root)

class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

with socketserver.TCPServer(("127.0.0.1", 0), Handler) as httpd:
    port = httpd.server_address[1]
    with open(port_file, "w", encoding="utf-8") as fh:
        fh.write(str(port))
    httpd.serve_forever()
PY
  HTTP_PID=$!
  for _ in $(seq 1 50); do
    if [ -s "$port_file" ]; then
      break
    fi
    sleep 0.05
  done
  [ -s "$port_file" ] || { echo "HTTP server failed to start" >&2; exit 1; }
  HTTP_PORT="$(cat "$port_file")"
}

stop_http() {
  if [ -n "${HTTP_PID:-}" ]; then
    kill "$HTTP_PID" 2>/dev/null || true
    wait "$HTTP_PID" 2>/dev/null || true
    HTTP_PID=""
  fi
}

PORT_FILE="$FIXTURE_ROOT/http.port"
HTTP_ROOT="$FIXTURE_ROOT/http"
mkdir -p "$HTTP_ROOT"

# --- Happy path ---
info "Happy path via local HTTP"
GOOD="$HTTP_ROOT/good"
mkdir -p "$GOOD/v0.1.0"
make_good_archive "$GOOD/v0.1.0" "v0.1.0" "$TARGET"
start_http "$GOOD" "$PORT_FILE"
INSTALL_DIR="$FIXTURE_ROOT/install-http"
mkdir -p "$INSTALL_DIR"
LOG="$FIXTURE_ROOT/http-happy.log"
if run_installer "http://127.0.0.1:$HTTP_PORT" "$INSTALL_DIR" "v0.1.0" "$LOG"; then
  if [ -x "$INSTALL_DIR/pummel" ]; then
    pass "HTTP happy path"
  else
    fail "HTTP happy path: binary missing"
    cat "$LOG" >&2 || true
  fi
else
  fail "HTTP happy path failed"
  cat "$LOG" >&2 || true
fi
stop_http
rm -f "$PORT_FILE"

# --- Bad checksum ---
info "Rejects bad checksum"
BAD="$HTTP_ROOT/bad-checksum"
mkdir -p "$BAD/v0.1.0"
make_good_archive "$BAD/v0.1.0" "v0.1.0" "$TARGET"
printf '0000000000000000000000000000000000000000000000000000000000000000  pummel-v0.1.0-%s.tar.gz\n' "$TARGET" \
  > "$BAD/v0.1.0/checksums-sha256.txt"
start_http "$BAD" "$PORT_FILE"
INSTALL_DIR="$FIXTURE_ROOT/install-bad"
mkdir -p "$INSTALL_DIR"
LOG="$FIXTURE_ROOT/bad.log"
if run_installer "http://127.0.0.1:$HTTP_PORT" "$INSTALL_DIR" "v0.1.0" "$LOG"; then
  fail "bad checksum should have failed"
else
  if grep -qi 'checksum' "$LOG"; then
    pass "bad checksum rejected"
  else
    fail "bad checksum failed for wrong reason"
    cat "$LOG" >&2 || true
  fi
fi
stop_http
rm -f "$PORT_FILE"

# --- Missing archive entry in checksums ---
info "Rejects missing archive entry in checksums"
MISS="$HTTP_ROOT/missing-entry"
mkdir -p "$MISS/v0.1.0"
make_good_archive "$MISS/v0.1.0" "v0.1.0" "$TARGET"
printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  other-file.tar.gz\n' \
  > "$MISS/v0.1.0/checksums-sha256.txt"
start_http "$MISS" "$PORT_FILE"
INSTALL_DIR="$FIXTURE_ROOT/install-miss"
mkdir -p "$INSTALL_DIR"
LOG="$FIXTURE_ROOT/miss.log"
if run_installer "http://127.0.0.1:$HTTP_PORT" "$INSTALL_DIR" "v0.1.0" "$LOG"; then
  fail "missing checksum entry should have failed"
else
  if grep -qiE 'not found in checksums|checksum' "$LOG"; then
    pass "missing checksum entry rejected"
  else
    fail "missing entry failed for wrong reason"
    cat "$LOG" >&2 || true
  fi
fi
stop_http
rm -f "$PORT_FILE"

# --- Wrong member name ---
info "Rejects unsafe tar member"
TRAV="$HTTP_ROOT/traversal"
mkdir -p "$TRAV/v0.1.0/payload"
printf 'evil\n' > "$TRAV/v0.1.0/payload/evil"
(
  cd "$TRAV/v0.1.0/payload"
  COPYFILE_DISABLE=1 tar -czf "../pummel-v0.1.0-${TARGET}.tar.gz" evil
)
ARCHIVE="pummel-v0.1.0-${TARGET}.tar.gz"
HASH="$(sha256sum "$TRAV/v0.1.0/$ARCHIVE" | awk '{ print $1 }')"
printf '%s  %s\n' "$HASH" "$ARCHIVE" > "$TRAV/v0.1.0/checksums-sha256.txt"
start_http "$TRAV" "$PORT_FILE"
INSTALL_DIR="$FIXTURE_ROOT/install-trav"
mkdir -p "$INSTALL_DIR"
LOG="$FIXTURE_ROOT/trav.log"
if run_installer "http://127.0.0.1:$HTTP_PORT" "$INSTALL_DIR" "v0.1.0" "$LOG"; then
  fail "unsafe tar member should have failed"
else
  if grep -qiE 'unexpected archive member|member validation|Archive must' "$LOG"; then
    pass "unsafe tar member rejected"
  else
    fail "unsafe member failed for wrong reason"
    cat "$LOG" >&2 || true
  fi
fi
stop_http
rm -f "$PORT_FILE"

# --- Symlink member ---
info "Rejects symlink tar member"
SYM="$HTTP_ROOT/symlink"
mkdir -p "$SYM/v0.1.0/payload"
ln -sf /etc/hosts "$SYM/v0.1.0/payload/pummel"
(
  cd "$SYM/v0.1.0/payload"
  # Store the symlink itself (do not follow with -h).
  COPYFILE_DISABLE=1 tar -czf "../pummel-v0.1.0-${TARGET}.tar.gz" pummel
)
ARCHIVE="pummel-v0.1.0-${TARGET}.tar.gz"
HASH="$(sha256sum "$SYM/v0.1.0/$ARCHIVE" | awk '{ print $1 }')"
printf '%s  %s\n' "$HASH" "$ARCHIVE" > "$SYM/v0.1.0/checksums-sha256.txt"
start_http "$SYM" "$PORT_FILE"
INSTALL_DIR="$FIXTURE_ROOT/install-sym"
mkdir -p "$INSTALL_DIR"
LOG="$FIXTURE_ROOT/sym.log"
if run_installer "http://127.0.0.1:$HTTP_PORT" "$INSTALL_DIR" "v0.1.0" "$LOG"; then
  fail "symlink member should have failed"
else
  if grep -qiE 'regular file|symlink|member validation|Archive member' "$LOG"; then
    pass "symlink tar member rejected"
  else
    fail "symlink member failed for wrong reason"
    cat "$LOG" >&2 || true
  fi
fi
stop_http
rm -f "$PORT_FILE"

printf '\nHarness summary: %s passed, %s failed\n' "$PASS" "$FAIL"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
if [ "$PASS" -lt 4 ]; then
  echo "Expected at least 4 passing checks" >&2
  exit 1
fi
