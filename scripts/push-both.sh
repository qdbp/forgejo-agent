#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: scripts/push-both.sh [<refspec>]

Pushes the current repo to:
1) origin (GitHub)
2) forgejo (local Forgejo mirror), using token auth for non-interactive push

Examples:
  scripts/push-both.sh
  scripts/push-both.sh HEAD
  scripts/push-both.sh main
EOF
  exit 2
}

[[ "${1:-}" == "--help" ]] && usage

repo_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[[ -n "${repo_root}" ]] || {
  echo "error: not inside a git repository" >&2
  exit 1
}

refspec="${1:-HEAD}"
forgejo_remote="${FORGEJO_PUSH_REMOTE:-forgejo}"
token_file="${FORGEJO_PUSH_TOKEN_FILE:-$HOME/.config/forgejo-agent/creds/codex-orch.token}"
askpass_tmp="$(mktemp)"

cleanup() {
  rm -f "$askpass_tmp"
}
trap cleanup EXIT

cat >"$askpass_tmp" <<'EOF'
#!/bin/sh
set -eu
cat "${ORCHD_GIT_TOKEN_FILE:?missing ORCHD_GIT_TOKEN_FILE}"
EOF
chmod 700 "$askpass_tmp"

git -C "$repo_root" remote get-url origin >/dev/null 2>&1 || {
  echo "error: origin remote is missing" >&2
  exit 1
}

git -C "$repo_root" remote get-url "$forgejo_remote" >/dev/null 2>&1 || {
  echo "error: forgejo remote '$forgejo_remote' is missing" >&2
  echo "hint: git remote add forgejo http://codex-orch@127.0.0.1:3000/main/forgejo-agent.git" >&2
  exit 1
}

[[ -r "$token_file" ]] || {
  echo "error: token file is not readable: $token_file" >&2
  exit 1
}

echo "[push-both] pushing origin ($refspec)"
git -C "$repo_root" push origin "$refspec"

echo "[push-both] pushing $forgejo_remote ($refspec)"
GIT_TERMINAL_PROMPT=0 GIT_ASKPASS="$askpass_tmp" ORCHD_GIT_TOKEN_FILE="$token_file" \
  git -C "$repo_root" push "$forgejo_remote" "$refspec"

echo "[push-both] done"
