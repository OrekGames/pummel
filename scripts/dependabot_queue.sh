#!/usr/bin/env bash
# Review open Dependabot pull requests and merge approved ones one at a time.
#
# evaluate: pure policy (no GitHub calls). Used by the harness and by `run`.
# run:      list / review / rebase / merge via the GitHub CLI.
set -euo pipefail

GH_BIN="${GH_BIN:-gh}"
REPO="${GITHUB_REPOSITORY:-OrekGames/pummel}"
DEFAULT_BASE="${DEPENDABOT_QUEUE_BASE:-main}"
MERGE_LIMIT="${DEPENDABOT_QUEUE_MERGE_LIMIT:-1}"
DRY_RUN="${DEPENDABOT_QUEUE_DRY_RUN:-0}"
BOT_LOGIN="${DEPENDABOT_QUEUE_BOT_LOGIN:-dependabot[bot]}"
REVIEW_MARKER="Dependabot queue:"

# Branch-protection required checks (ruleset "main protection").
REQUIRED_CHECKS=(
  fmt
  clippy
  test
  examples
  msrv
  docs
  docker-smoke
  installer-checks
  audit
)

ALLOWED_FILE_RE='^(Cargo\.toml|Cargo\.lock|\.github/dependabot\.yml|\.github/workflows/[^/]+\.ya?ml)$'

usage() {
  cat <<'EOF'
Usage:
  scripts/dependabot_queue.sh evaluate [options]
  scripts/dependabot_queue.sh run [--dry-run]

evaluate options:
  --author LOGIN
  --base BRANCH
  --update-type TYPE          Repeatable. Dependabot commit trailer, if any.
  --file PATH                 Repeatable. Changed paths on the PR.
  --label NAME                Repeatable. PR labels.
  --security                  Treat as a Dependabot security update.

evaluate prints APPROVE or REJECT, then a reason.
EOF
}

is_truthy() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|on|ON) return 0 ;;
    *) return 1 ;;
  esac
}

cmd_evaluate() {
  local author=""
  local base=""
  local security=0
  local -a update_types=()
  local -a files=()
  local -a labels=()

  while [ $# -gt 0 ]; do
    case "$1" in
      --author)
        author="${2:-}"
        shift 2
        ;;
      --base)
        base="${2:-}"
        shift 2
        ;;
      --update-type)
        update_types+=("${2:-}")
        shift 2
        ;;
      --file)
        files+=("${2:-}")
        shift 2
        ;;
      --label)
        labels+=("${2:-}")
        shift 2
        ;;
      --security)
        security=1
        shift
        ;;
      -h|--help)
        usage
        return 0
        ;;
      *)
        echo "Unknown evaluate option: $1" >&2
        return 2
        ;;
    esac
  done

  if [ "$author" != "$BOT_LOGIN" ]; then
    reject "author is '${author:-<empty>}', expected ${BOT_LOGIN}"
    return 0
  fi

  if [ "$base" != "$DEFAULT_BASE" ]; then
    reject "base is '${base:-<empty>}', expected ${DEFAULT_BASE}"
    return 0
  fi

  if [ ${#files[@]} -eq 0 ]; then
    reject "PR changes no files"
    return 0
  fi

  local file cargo_files=0 workflow_files=0
  for file in "${files[@]}"; do
    if ! [[ "$file" =~ $ALLOWED_FILE_RE ]]; then
      reject "unexpected path '${file}'"
      return 0
    fi
    case "$file" in
      Cargo.toml|Cargo.lock) cargo_files=1 ;;
      .github/workflows/*) workflow_files=1 ;;
    esac
  done

  if [ "$cargo_files" -eq 1 ] && [ "$workflow_files" -eq 1 ]; then
    reject "mixed Cargo and GitHub Actions changes"
    return 0
  fi

  local label
  for label in "${labels[@]}"; do
    case "$(printf '%s' "$label" | tr '[:upper:]' '[:lower:]')" in
      security) security=1 ;;
    esac
  done

  local -a normalized=()
  local ut
  for ut in "${update_types[@]}"; do
    [ -n "$ut" ] || continue
    normalized+=("$ut")
  done

  if [ ${#normalized[@]} -eq 0 ]; then
    if [ "$workflow_files" -eq 1 ]; then
      approve "GitHub Actions pin/digest update"
      return 0
    fi
    reject "Cargo update is missing a Dependabot update-type trailer"
    return 0
  fi

  for ut in "${normalized[@]}"; do
    case "$ut" in
      version-update:semver-patch|version-update:semver-minor)
        ;;
      version-update:semver-major)
        if [ "$security" -eq 1 ]; then
          continue
        fi
        reject "major version update requires a human review"
        return 0
        ;;
      *)
        reject "unsupported update-type '${ut}'"
        return 0
        ;;
    esac
  done

  if [ "$security" -eq 1 ]; then
    approve "allowed Dependabot update (security)"
  else
    approve "allowed Dependabot update"
  fi
}

approve() {
  printf 'APPROVE\n%s\n' "$1"
}

reject() {
  printf 'REJECT\n%s\n' "$1"
}

gh_cmd() {
  "$GH_BIN" "$@"
}

# Parse `gh pr view --json ...` into exported pr_* variables.
load_pr_meta() {
  local view="$1"
  local meta
  meta="$(python3 -c '
import json, re, sys

pr = json.load(sys.stdin)
author = (pr.get("author") or {}).get("login") or ""
files = [f.get("path") or "" for f in (pr.get("files") or [])]
labels = [l.get("name") or "" for l in (pr.get("labels") or [])]
text = "\n".join(
    (c.get("messageHeadline") or "") + "\n" + (c.get("messageBody") or "")
    for c in (pr.get("commits") or [])
)
update_types = re.findall(r"(?m)^[ \t]*update-type:[ \t]*(\S+)", text)
security = any((label or "").lower() == "security" for label in labels) or bool(
    re.search(r"\b(GHSA-[0-9a-z-]+|CVE-\d{4}-\d+)\b", text, re.I)
    or re.search(r"dependabot security update", text, re.I)
)
approved = any(
    (review.get("state") == "APPROVED")
    and ((review.get("author") or {}).get("login") or "")
    in {"github-actions[bot]", "github-actions"}
    for review in (pr.get("reviews") or [])
)
checks = [
    {
        "name": check.get("name") or "",
        "status": check.get("status") or "",
        "conclusion": check.get("conclusion") or "",
    }
    for check in (pr.get("statusCheckRollup") or [])
]
print(json.dumps({
    "author": author,
    "base": pr.get("baseRefName") or "",
    "draft": "1" if pr.get("isDraft") else "0",
    "files": files,
    "labels": labels,
    "update_types": update_types,
    "security": "1" if security else "0",
    "approved": "1" if approved else "0",
    "merge_state": pr.get("mergeStateStatus") or "",
    "title": pr.get("title") or "",
    "url": pr.get("url") or "",
    "checks": checks,
}))
' <<<"$view")"

  eval "$(python3 -c '
import json, shlex, sys
meta = json.loads(sys.argv[1])
print("pr_author=" + shlex.quote(meta["author"]))
print("pr_base=" + shlex.quote(meta["base"]))
print("pr_draft=" + shlex.quote(meta["draft"]))
print("pr_security=" + shlex.quote(meta["security"]))
print("pr_approved=" + shlex.quote(meta["approved"]))
print("pr_merge_state=" + shlex.quote(meta["merge_state"]))
print("pr_title=" + shlex.quote(meta["title"]))
print("pr_url=" + shlex.quote(meta["url"]))
print("pr_files=" + shlex.quote("\n".join(meta["files"])))
print("pr_labels=" + shlex.quote("\n".join(meta["labels"])))
print("pr_update_types=" + shlex.quote("\n".join(meta["update_types"])))
print("pr_checks_json=" + shlex.quote(json.dumps(meta["checks"])))
' "$meta")"
}

evaluate_loaded_pr() {
  local -a eval_args=(--author "$pr_author" --base "$pr_base")
  local path ut label
  while IFS= read -r path; do
    [ -n "$path" ] || continue
    eval_args+=(--file "$path")
  done <<<"$pr_files"
  while IFS= read -r ut; do
    [ -n "$ut" ] || continue
    eval_args+=(--update-type "$ut")
  done <<<"$pr_update_types"
  while IFS= read -r label; do
    [ -n "$label" ] || continue
    eval_args+=(--label "$label")
  done <<<"$pr_labels"
  if [ "$pr_security" = "1" ]; then
    eval_args+=(--security)
  fi
  cmd_evaluate "${eval_args[@]}"
}

cmd_run() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run)
        DRY_RUN=1
        shift
        ;;
      -h|--help)
        usage
        return 0
        ;;
      *)
        echo "Unknown run option: $1" >&2
        return 2
        ;;
    esac
  done

  if ! command -v "$GH_BIN" >/dev/null 2>&1; then
    echo "Missing GitHub CLI: $GH_BIN" >&2
    return 1
  fi
  if ! command -v python3 >/dev/null 2>&1; then
    echo "Missing required command: python3" >&2
    return 1
  fi

  local pr_json
  pr_json="$(gh_cmd pr list --repo "$REPO" --state open --author "app/dependabot" \
    --limit 100 --json number,title,url)"

  local -a numbers=()
  mapfile -t numbers < <(python3 -c '
import json, sys
prs = json.load(sys.stdin)
for pr in sorted(prs, key=lambda item: item["number"]):
    print(pr["number"])
' <<<"$pr_json")

  if [ ${#numbers[@]} -eq 0 ] || [ -z "${numbers[0]:-}" ]; then
    echo "No open Dependabot pull requests."
    return 0
  fi

  local merged=0
  local number
  for number in "${numbers[@]}"; do
    review_one "$number" || true
  done

  for number in "${numbers[@]}"; do
    if [ "$merged" -ge "$MERGE_LIMIT" ]; then
      break
    fi
    if try_merge_one "$number"; then
      merged=$((merged + 1))
    fi
  done

  echo "Reviewed ${#numbers[@]} Dependabot PR(s); merged ${merged}."
}

fetch_pr_view() {
  gh_cmd pr view "$1" --repo "$REPO" --json \
    number,title,url,author,baseRefName,isDraft,files,commits,labels,reviews,mergeStateStatus,statusCheckRollup
}

review_one() {
  local number="$1"
  load_pr_meta "$(fetch_pr_view "$number")"

  if [ "$pr_draft" = "1" ]; then
    echo "PR #${number}: skip draft"
    return 0
  fi

  local result decision reason
  result="$(evaluate_loaded_pr)"
  decision="$(printf '%s\n' "$result" | sed -n '1p')"
  reason="$(printf '%s\n' "$result" | sed -n '2p')"

  echo "PR #${number} (${pr_title}): ${decision} — ${reason}"

  if [ "$decision" = "APPROVE" ]; then
    if [ "$pr_approved" = "1" ]; then
      echo "PR #${number}: already approved"
      return 0
    fi
    submit_review "$number" APPROVE "${REVIEW_MARKER} ${reason}."
  else
    comment_once "$number" "${REVIEW_MARKER} not approved: ${reason}."
  fi
}

submit_review() {
  local number="$1"
  local event="$2"
  local body="$3"
  if is_truthy "$DRY_RUN"; then
    echo "DRY-RUN: review ${event} #${number}: ${body}"
    return 0
  fi
  gh_cmd api --method POST "repos/${REPO}/pulls/${number}/reviews" \
    -f event="$event" -f body="$body" >/dev/null
  echo "Submitted ${event} on #${number}"
}

comment_once() {
  local number="$1"
  local body="$2"
  local existing
  existing="$(gh_cmd api "repos/${REPO}/issues/${number}/comments" --paginate \
    --jq '.[].body' 2>/dev/null || true)"
  if printf '%s\n' "$existing" | grep -Fxq "$body"; then
    echo "PR #${number}: comment already present"
    return 0
  fi
  if is_truthy "$DRY_RUN"; then
    echo "DRY-RUN: comment #${number}: ${body}"
    return 0
  fi
  gh_cmd pr comment "$number" --repo "$REPO" --body "$body" >/dev/null
  echo "Commented on #${number}"
}

checks_ready() {
  local checks_json="$1"
  python3 -c '
import json, sys

required = sys.argv[1].split(",")
checks = json.loads(sys.argv[2])
by_name = {}
for check in checks:
    name = check.get("name") or ""
    if name:
        by_name[name] = check

pending = []
failed = []
missing = []
for name in required:
    check = by_name.get(name)
    if check is None:
        missing.append(name)
        continue
    status = (check.get("status") or "").upper()
    conclusion = (check.get("conclusion") or "").upper()
    if status != "COMPLETED":
        pending.append(name)
    elif conclusion != "SUCCESS":
        failed.append(f"{name}:{conclusion or status}")

if missing:
    print("missing:" + ",".join(missing))
    raise SystemExit(2)
if pending:
    print("pending:" + ",".join(pending))
    raise SystemExit(3)
if failed:
    print("failed:" + ",".join(failed))
    raise SystemExit(4)
print("ok")
' "$(IFS=','; echo "${REQUIRED_CHECKS[*]}")" "$checks_json"
}

try_merge_one() {
  local number="$1"
  load_pr_meta "$(fetch_pr_view "$number")"

  if [ "$pr_draft" = "1" ]; then
    return 1
  fi

  local result decision
  result="$(evaluate_loaded_pr)"
  decision="$(printf '%s\n' "$result" | sed -n '1p')"
  if [ "$decision" != "APPROVE" ]; then
    echo "PR #${number}: not merging (${decision})"
    return 1
  fi

  # Live runs approve before this loop. Dry-run cannot persist that approval.
  if [ "$pr_approved" != "1" ] && ! is_truthy "$DRY_RUN"; then
    echo "PR #${number}: waiting for approval to land"
    return 1
  fi

  case "$pr_merge_state" in
    DIRTY)
      echo "PR #${number}: conflicts; requesting Dependabot recreate"
      comment_once "$number" "@dependabot recreate"
      return 1
      ;;
    CLEAN|UNSTABLE|HAS_HOOKS)
      ;;
    DRAFT)
      return 1
      ;;
    *)
      echo "PR #${number}: not up to date (${pr_merge_state:-empty}); requesting rebase"
      comment_once "$number" "@dependabot rebase"
      return 1
      ;;
  esac

  local check_state
  if ! check_state="$(checks_ready "$pr_checks_json")"; then
    echo "PR #${number}: checks not ready (${check_state:-unknown})"
    return 1
  fi

  if is_truthy "$DRY_RUN"; then
    echo "DRY-RUN: merge #${number} (${pr_title})"
    return 0
  fi

  gh_cmd pr merge "$number" --repo "$REPO" --squash --delete-branch
  echo "Merged #${number} (${pr_title})"
  return 0
}

case "${1:-}" in
  evaluate)
    shift
    cmd_evaluate "$@"
    ;;
  run)
    shift
    cmd_run "$@"
    ;;
  -h|--help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
