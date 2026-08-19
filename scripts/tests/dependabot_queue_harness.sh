#!/usr/bin/env bash
# Offline harness for scripts/dependabot_queue.sh (evaluate + mocked run).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QUEUE="$ROOT/scripts/dependabot_queue.sh"
PASS=0
FAIL=0

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

expect_eval() {
  local name="$1"
  local want="$2"
  shift 2
  local got
  got="$("$QUEUE" evaluate "$@" | sed -n '1p')"
  if [ "$got" = "$want" ]; then
    pass "$name"
  else
    fail "$name (got ${got}, want ${want})"
  fi
}

info "evaluate policy"

expect_eval "patch cargo lock" APPROVE \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock --update-type version-update:semver-patch

expect_eval "minor cargo toml+lock" APPROVE \
  --author "dependabot[bot]" --base main \
  --file Cargo.toml --file Cargo.lock \
  --update-type version-update:semver-minor

expect_eval "major cargo without security" REJECT \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock --update-type version-update:semver-major

expect_eval "major cargo with security flag" APPROVE \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock --update-type version-update:semver-major \
  --security

expect_eval "major cargo with security label" APPROVE \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock --update-type version-update:semver-major \
  --label security

expect_eval "grouped patch plus major" REJECT \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock \
  --update-type version-update:semver-patch \
  --update-type version-update:semver-major

expect_eval "actions digest without update-type" APPROVE \
  --author "dependabot[bot]" --base main \
  --file .github/workflows/release.yml

expect_eval "cargo without update-type" REJECT \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock

expect_eval "unexpected source file" REJECT \
  --author "dependabot[bot]" --base main \
  --file src/lib.rs --update-type version-update:semver-patch

expect_eval "mixed cargo and workflow" REJECT \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock --file .github/workflows/ci.yml \
  --update-type version-update:semver-patch

expect_eval "wrong author" REJECT \
  --author "alice" --base main \
  --file Cargo.lock --update-type version-update:semver-patch

expect_eval "wrong base" REJECT \
  --author "dependabot[bot]" --base develop \
  --file Cargo.lock --update-type version-update:semver-patch

expect_eval "no files" REJECT \
  --author "dependabot[bot]" --base main \
  --update-type version-update:semver-patch

expect_eval "unsupported update-type" REJECT \
  --author "dependabot[bot]" --base main \
  --file Cargo.lock --update-type version-update:mystery

info "mocked run"

FAKE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pummel-dependabot-queue.XXXXXX")"
cleanup() {
  rm -rf "$FAKE_ROOT"
}
trap cleanup EXIT

write_green_checks() {
  python3 -c '
import json, sys
names = [
    "fmt", "clippy", "test", "examples", "msrv", "docs",
    "docker-smoke", "installer-checks", "audit",
]
print(json.dumps([
    {"name": name, "status": "COMPLETED", "conclusion": "SUCCESS"}
    for name in names
]))
'
}

write_pr() {
  local dest="$1"
  local number="$2"
  local merge_state="$3"
  local update_type="$4"
  local approved="$5"
  local file_path="$6"
  python3 -c '
import json, sys

number = int(sys.argv[1])
merge_state = sys.argv[2]
update_type = sys.argv[3]
approved = sys.argv[4] == "1"
file_path = sys.argv[5]
checks = json.loads(sys.argv[6])
body = ""
if update_type:
    body = f"updated-dependencies:\n- dependency-name: demo\n  update-type: {update_type}\n"
reviews = []
if approved:
    reviews.append({"state": "APPROVED", "author": {"login": "github-actions[bot]"}})
pr = {
    "number": number,
    "title": f"build(deps): bump demo #{number}",
    "url": f"https://example.test/{number}",
    "author": {"login": "dependabot[bot]"},
    "baseRefName": "main",
    "isDraft": False,
    "files": [{"path": file_path}],
    "commits": [{"messageHeadline": "build(deps): bump demo", "messageBody": body}],
    "labels": [{"name": "dependencies"}],
    "reviews": reviews,
    "mergeStateStatus": merge_state,
    "statusCheckRollup": checks,
}
print(json.dumps(pr))
' "$number" "$merge_state" "$update_type" "$approved" "$file_path" "$(write_green_checks)" > "$dest"
}

install_fake_gh() {
  local store="$1"
  cat > "$store/gh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
STORE="${FAKE_GH_STORE:?}"
log="$STORE/commands.log"
printf '%s\n' "$*" >> "$log"

if [ "${1:-}" = "pr" ] && [ "${2:-}" = "list" ]; then
  python3 -c '
import json, pathlib, sys
store = pathlib.Path(sys.argv[1])
items = []
for path in sorted(store.glob("pr-*.json")):
    pr = json.loads(path.read_text())
    items.append({"number": pr["number"], "title": pr["title"], "url": pr["url"]})
print(json.dumps(items))
' "$STORE"
  exit 0
fi

if [ "${1:-}" = "pr" ] && [ "${2:-}" = "view" ]; then
  number="$3"
  cat "$STORE/pr-${number}.json"
  exit 0
fi

if [ "${1:-}" = "api" ]; then
  # reviews POST or comments GET
  if [[ "$*" == *"/reviews"* ]]; then
    echo '{"id":1}'
    exit 0
  fi
  echo '[]'
  exit 0
fi

if [ "${1:-}" = "pr" ] && [ "${2:-}" = "comment" ]; then
  echo '{"id":1}'
  exit 0
fi

if [ "${1:-}" = "pr" ] && [ "${2:-}" = "merge" ]; then
  printf '%s\n' "$3" >> "$STORE/merged.txt"
  echo "merged $3"
  exit 0
fi

echo "unexpected gh invocation: $*" >&2
exit 1
EOF
  chmod +x "$store/gh"
}

run_queue() {
  local store="$1"
  shift
  FAKE_GH_STORE="$store" GH_BIN="$store/gh" GITHUB_REPOSITORY="OrekGames/pummel" \
    "$QUEUE" run "$@"
}

STORE="$FAKE_ROOT/behind"
mkdir -p "$STORE"
write_pr "$STORE/pr-141.json" 141 BEHIND "" 0 ".github/workflows/release.yml"
write_pr "$STORE/pr-142.json" 142 CLEAN "version-update:semver-patch" 0 "Cargo.lock"
install_fake_gh "$STORE"

behind_out="$(run_queue "$STORE" --dry-run)"
if printf '%s\n' "$behind_out" | grep -q 'DRY-RUN: merge #142'; then
  pass "dry-run merges first clean approved candidate"
else
  fail "dry-run did not select #142"
  printf '%s\n' "$behind_out" >&2
fi
if printf '%s\n' "$behind_out" | grep -q 'DRY-RUN: merge #141'; then
  fail "dry-run merged behind PR #141"
else
  pass "dry-run skipped behind PR #141"
fi
if [ -f "$STORE/merged.txt" ]; then
  fail "dry-run wrote a merge"
else
  pass "dry-run performed no merge"
fi

STORE="$FAKE_ROOT/live"
mkdir -p "$STORE"
write_pr "$STORE/pr-141.json" 141 BEHIND "" 1 ".github/workflows/release.yml"
write_pr "$STORE/pr-142.json" 142 CLEAN "version-update:semver-patch" 1 "Cargo.lock"
write_pr "$STORE/pr-143.json" 143 CLEAN "version-update:semver-patch" 1 "Cargo.lock"
install_fake_gh "$STORE"

live_out="$(run_queue "$STORE")"
if [ -f "$STORE/merged.txt" ] && [ "$(tr '\n' ' ' < "$STORE/merged.txt")" = "142 " ]; then
  pass "live run merges exactly the next ready PR"
else
  fail "live run merge set was unexpected"
  printf '%s\n' "$live_out" >&2
  cat "$STORE/merged.txt" >&2 || true
fi
if printf '%s\n' "$live_out" | grep -q '@dependabot rebase' || grep -q 'not up to date' <<<"$live_out"; then
  pass "live run requests rebase for the behind PR"
else
  fail "live run did not request rebase for #141"
  printf '%s\n' "$live_out" >&2
fi

STORE="$FAKE_ROOT/major"
mkdir -p "$STORE"
write_pr "$STORE/pr-150.json" 150 CLEAN "version-update:semver-major" 0 "Cargo.lock"
install_fake_gh "$STORE"
major_out="$(run_queue "$STORE")"
if [ -f "$STORE/merged.txt" ]; then
  fail "major update was merged"
else
  pass "major update was not merged"
fi
if printf '%s\n' "$major_out" | grep -q 'REJECT.*major version update requires a human review' \
  && printf '%s\n' "$major_out" | grep -q 'Commented on #150'; then
  pass "major update left a rejection comment"
else
  fail "major update was not rejected with a comment"
  printf '%s\n' "$major_out" >&2
fi

echo
echo "Passed: $PASS  Failed: $FAIL"
if [ "$FAIL" -ne 0 ]; then
  exit 1
fi
