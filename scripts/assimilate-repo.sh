#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/common.sh
source "$SCRIPT_DIR/../lib/common.sh"

need_cmd git
need_cmd rg
need_cmd jq

FORGEJOCTL_BIN="${FORGEJOCTL_BIN:-$HOME/.local/bin/forgejoctl}"
ORCHD_BIN="${ORCHD_BIN:-$HOME/.local/bin/orchd}"
ORCHD_TOKEN_FILE="${ORCHD_TOKEN_FILE:-$HOME/.config/forgejo-agent/creds/orchd.token}"
DISPATCH_CONFIG_DEFAULT="$SCRIPT_DIR/../config/orchd-dispatch.toml"
AGENTS_SNIPPET_TEMPLATE="$SCRIPT_DIR/../templates/repo-agents-assimilation-snippet.md"

usage() {
  cat >&2 <<'EOF'
usage: scripts/assimilate-repo.sh --repo owner/repo --local-path /abs/path/to/repo [options]

End-to-end repo assimilation for orchd dispatch:
- ensures Forgejo repo + policy labels
- wires local git forgejo remote (and bootstraps main branch when absent)
- appends repo binding to orchd dispatch config
- injects repo-local AGENTS.md assimilation snippet

options:
  --repo OWNER/REPO              required
  --local-path PATH              required (local git checkout)
  --dispatch-config PATH         default: config/orchd-dispatch.toml
  --dispatch-git-remote NAME     default: origin
  --dispatch-git-base BRANCH     default: main
  --forgejo-remote NAME          default: forgejo
  --forgejo-login LOGIN          default: codex-orch
  --orch-login LOGIN             default: codex-orch
  --lead-login LOGIN             default: codex-lead
  --dev-login LOGIN              default: codex-dev
  --skip-acl                     do not apply repo-scoped collaborator ACLs
  --skip-role-check-preflight    skip `orchd role check` preflight gate
  --bootstrap-token-file PATH    default: ~/.config/forgejo-agent/creds/codex-orch.token
  --skip-bootstrap-push          do not push initial branch to forgejo remote
  --skip-agents-patch            do not patch repo-local AGENTS.md
  --dry-run                      print planned mutations without writing
  --help
EOF
  exit 2
}

repo_ref=""
local_path=""
dispatch_config="$DISPATCH_CONFIG_DEFAULT"
dispatch_git_remote="origin"
dispatch_git_base="main"
forgejo_remote="forgejo"
forgejo_login="codex-orch"
orch_login="codex-orch"
lead_login="codex-lead"
dev_login="codex-dev"
apply_acl=1
skip_role_check_preflight=0
bootstrap_token_file="$HOME/.config/forgejo-agent/creds/codex-orch.token"
bootstrap_push=1
patch_agents=1
dry_run=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo) repo_ref="${2-}"; shift 2 ;;
    --local-path) local_path="${2-}"; shift 2 ;;
    --dispatch-config) dispatch_config="${2-}"; shift 2 ;;
    --dispatch-git-remote) dispatch_git_remote="${2-}"; shift 2 ;;
    --dispatch-git-base) dispatch_git_base="${2-}"; shift 2 ;;
    --forgejo-remote) forgejo_remote="${2-}"; shift 2 ;;
    --forgejo-login) forgejo_login="${2-}"; shift 2 ;;
    --orch-login) orch_login="${2-}"; shift 2 ;;
    --lead-login) lead_login="${2-}"; shift 2 ;;
    --dev-login) dev_login="${2-}"; shift 2 ;;
    --skip-acl) apply_acl=0; shift ;;
    --skip-role-check-preflight) skip_role_check_preflight=1; shift ;;
    --bootstrap-token-file) bootstrap_token_file="${2-}"; shift 2 ;;
    --skip-bootstrap-push) bootstrap_push=0; shift ;;
    --skip-agents-patch) patch_agents=0; shift ;;
    --dry-run) dry_run=1; shift ;;
    --help|-h) usage ;;
    *) err "unknown argument: $1" ;;
  esac
done

[[ -n "$repo_ref" ]] || err "--repo is required"
[[ -n "$local_path" ]] || err "--local-path is required"
parse_repo_only "$repo_ref"
repo_ref="${REPO_OWNER}/${REPO_NAME}"

local_path="$(expand_tilde "$local_path")"
dispatch_config="$(expand_tilde "$dispatch_config")"
bootstrap_token_file="$(expand_tilde "$bootstrap_token_file")"

[[ -f "$dispatch_config" ]] || err "dispatch config not found: $dispatch_config"
[[ -f "$AGENTS_SNIPPET_TEMPLATE" ]] || err "missing AGENTS snippet template: $AGENTS_SNIPPET_TEMPLATE"
[[ -x "$FORGEJOCTL_BIN" ]] || err "forgejoctl binary is not executable: $FORGEJOCTL_BIN"
if [[ "$skip_role_check_preflight" -ne 1 ]]; then
  [[ -x "$ORCHD_BIN" ]] || err "orchd binary is not executable: $ORCHD_BIN"
  [[ -r "$ORCHD_TOKEN_FILE" ]] || err "orchd token file is not readable: $ORCHD_TOKEN_FILE"
fi

if [[ ! -d "$local_path" ]]; then
  err "local path does not exist: $local_path"
fi
local_path="$(cd "$local_path" && pwd -P)"

git -C "$local_path" rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
  err "local path is not a git checkout: $local_path"
}

load_config

base_url="${FORGEJO_BASE_URL%/}"
case "$base_url" in
  http://*)
    forgejo_push_url="http://${forgejo_login}@${base_url#http://}/${repo_ref}.git"
    ;;
  https://*)
    forgejo_push_url="https://${forgejo_login}@${base_url#https://}/${repo_ref}.git"
    ;;
  *)
    err "unsupported FORGEJO_BASE_URL (expected http[s]://...): $FORGEJO_BASE_URL"
    ;;
esac

log() {
  printf '[assimilation] %s\n' "$*"
}

run() {
  if [[ "$dry_run" -eq 1 ]]; then
    printf '[dry-run] %s\n' "$*"
  else
    "$@"
  fi
}

run_role_check_preflight() {
  if [[ "$skip_role_check_preflight" -eq 1 ]]; then
    log "skipping role-check preflight by request"
    return
  fi
  log "running orchd role integrity preflight"
  run "$ORCHD_BIN" \
    --token-file "$ORCHD_TOKEN_FILE" \
    --dispatch-config "$dispatch_config" \
    role check
}

append_text() {
  local file="$1"
  local text="$2"
  if [[ "$dry_run" -eq 1 ]]; then
    printf '[dry-run] append to %s:\n%s\n' "$file" "$text"
  else
    printf '%s\n' "$text" >> "$file"
  fi
}

ensure_repo_labels() {
  log "ensuring Forgejo repo + policy labels for $repo_ref"
  run "$FORGEJOCTL_BIN" repo ensure "$repo_ref"
}

apply_collaborator_acl() {
  if [[ "$apply_acl" -ne 1 ]]; then
    log "skipping collaborator ACL setup by request"
    return
  fi

  log "applying repo-scoped collaborator ACLs"

  set_acl() {
    local login="$1"
    local permission="$2"
    if [[ -z "$login" || "$login" == "$REPO_OWNER" ]]; then
      return
    fi
    local payload
    payload="$(jq -cn --arg perm "$permission" '{permission:$perm}')"
    if [[ "$dry_run" -eq 1 ]]; then
      printf '[dry-run] PUT /api/v1/repos/%s/%s/collaborators/%s %s\n' \
        "$REPO_OWNER" "$REPO_NAME" "$login" "$payload"
    else
      forgejo_api PUT "/api/v1/repos/$REPO_OWNER/$REPO_NAME/collaborators/$login" "$payload" >/dev/null
    fi
  }

  set_acl "$orch_login" "admin"
  set_acl "$lead_login" "admin"
  set_acl "$dev_login" "write"
}

ensure_local_remote() {
  local existing=""
  if existing="$(git -C "$local_path" remote get-url "$forgejo_remote" 2>/dev/null)"; then
    if [[ "$existing" == "$forgejo_push_url" ]]; then
      log "git remote '$forgejo_remote' already points to $forgejo_push_url"
      return
    fi
    log "updating git remote '$forgejo_remote' url"
    run git -C "$local_path" remote set-url "$forgejo_remote" "$forgejo_push_url"
    return
  fi

  log "adding git remote '$forgejo_remote' -> $forgejo_push_url"
  run git -C "$local_path" remote add "$forgejo_remote" "$forgejo_push_url"
}

setup_askpass() {
  [[ -r "$bootstrap_token_file" ]] || err "bootstrap token file is not readable: $bootstrap_token_file"
  askpass_tmp="$(mktemp)"
  cat > "$askpass_tmp" <<'EOF'
#!/bin/sh
set -eu
cat "${ORCHD_GIT_TOKEN_FILE:?missing ORCHD_GIT_TOKEN_FILE}"
EOF
  chmod 700 "$askpass_tmp"
}

cleanup_askpass() {
  if [[ -n "${askpass_tmp:-}" ]]; then
    rm -f "$askpass_tmp"
  fi
}
trap cleanup_askpass EXIT

git_with_token() {
  GIT_TERMINAL_PROMPT=0 \
    GIT_ASKPASS="$askpass_tmp" \
    ORCHD_GIT_TOKEN_FILE="$bootstrap_token_file" \
    git "$@"
}

bootstrap_remote_branch_if_needed() {
  if [[ "$bootstrap_push" -ne 1 ]]; then
    log "skipping bootstrap push by request"
    return
  fi

  if ! git -C "$local_path" show-ref --verify --quiet "refs/heads/$dispatch_git_base"; then
    err "local branch '$dispatch_git_base' not found in $local_path"
  fi

  setup_askpass

  local ls_out=""
  if ! ls_out="$(git_with_token -C "$local_path" ls-remote --heads "$forgejo_remote" "$dispatch_git_base" 2>/dev/null || true)"; then
    ls_out=""
  fi
  if [[ -n "$ls_out" ]]; then
    log "forgejo branch '$dispatch_git_base' already exists; bootstrap push not needed"
    return
  fi

  log "bootstrapping forgejo branch '$dispatch_git_base' from local checkout"
  if [[ "$dry_run" -eq 1 ]]; then
    printf '[dry-run] git push %s %s:%s (token askpass)\n' \
      "$forgejo_remote" "$dispatch_git_base" "$dispatch_git_base"
  else
    git_with_token -C "$local_path" push "$forgejo_remote" "${dispatch_git_base}:${dispatch_git_base}"
  fi
}

ensure_dispatch_binding() {
  local binding_key="repo = \"$repo_ref\""
  if rg -Fq "$binding_key" "$dispatch_config"; then
    log "dispatch binding already present for $repo_ref in $(basename "$dispatch_config")"
    return
  fi

  local backup="${dispatch_config}.bak.$(date +%Y%m%d-%H%M%S)"
  log "adding dispatch repo binding for $repo_ref (backup: $backup)"

  if [[ "$dry_run" -eq 0 ]]; then
    cp -a "$dispatch_config" "$backup"
  else
    printf '[dry-run] cp -a %s %s\n' "$dispatch_config" "$backup"
  fi

  local block
  block="$(cat <<EOF

[[repo_bindings]]
repo = "$repo_ref"
local_path = "$local_path"
git_remote = "$dispatch_git_remote"
git_base = "$dispatch_git_base"
EOF
)"
  append_text "$dispatch_config" "$block"
}

ensure_repo_agents_snippet() {
  if [[ "$patch_agents" -ne 1 ]]; then
    log "skipping AGENTS.md patch by request"
    return
  fi

  local agents_file="$local_path/AGENTS.md"
  local marker_start="<!-- forgejo-assimilation:start -->"
  local marker_end="<!-- forgejo-assimilation:end -->"

  if [[ -f "$agents_file" ]] && rg -Fq "$marker_start" "$agents_file"; then
    log "AGENTS.md already contains assimilation snippet markers"
    return
  fi

  local snippet=""
  snippet="$(sed "s|{{repo_ref}}|$repo_ref|g" "$AGENTS_SNIPPET_TEMPLATE")"
  local payload
  payload="$(cat <<EOF

$marker_start
$snippet
$marker_end
EOF
)"

  if [[ ! -f "$agents_file" ]]; then
    log "creating $agents_file"
    if [[ "$dry_run" -eq 1 ]]; then
      printf '[dry-run] create %s with assimilation snippet\n' "$agents_file"
    else
      : > "$agents_file"
    fi
  else
    log "patching $agents_file"
  fi
  append_text "$agents_file" "$payload"
}

print_next_steps() {
  cat <<EOF

[assimilation] complete for $repo_ref

next steps:
1. Review + commit updated files:
   - $dispatch_config
   - $local_path/AGENTS.md (if patched)
2. Restart orchd user service:
   XDG_RUNTIME_DIR=/run/user/\$(id -u) DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/\$(id -u)/bus systemctl --user restart orchd.service
3. Smoke-test dispatch:
   - open issue in $repo_ref with '@codex-dev poke'
   - verify orchd/state labels progress and reply appears
EOF
}

log "starting assimilation for $repo_ref"
log "local path: $local_path"
log "dispatch config: $dispatch_config"
log "forgejo remote: $forgejo_remote ($forgejo_push_url)"

run_role_check_preflight
ensure_repo_labels
apply_collaborator_acl
ensure_local_remote
bootstrap_remote_branch_if_needed
ensure_dispatch_binding
ensure_repo_agents_snippet
print_next_steps
