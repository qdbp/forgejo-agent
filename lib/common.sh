#!/usr/bin/env bash
set -euo pipefail

FORGEJO_AGENT_CONFIG_DIR="${FORGEJO_AGENT_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/forgejo-agent}"
FORGEJO_AGENT_CONFIG_FILE="${FORGEJO_AGENT_CONFIG_FILE:-$FORGEJO_AGENT_CONFIG_DIR/config.env}"
FORGEJO_AGENT_TOKEN_FILE_DEFAULT="$FORGEJO_AGENT_CONFIG_DIR/token"

err() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"
}

expand_tilde() {
  local value="$1"
  if [[ "$value" == "~" ]]; then
    printf '%s\n' "$HOME"
    return
  fi
  if [[ "$value" == ~/* ]]; then
    printf '%s\n' "$HOME/${value#~/}"
    return
  fi
  printf '%s\n' "$value"
}

load_config() {
  [[ -f "$FORGEJO_AGENT_CONFIG_FILE" ]] || err "missing config file: $FORGEJO_AGENT_CONFIG_FILE"

  # shellcheck disable=SC1090
  source "$FORGEJO_AGENT_CONFIG_FILE"

  : "${FORGEJO_BASE_URL:=http://127.0.0.1:3000}"
  : "${FORGEJO_DEFAULT_OWNER:=main}"
  : "${FORGEJO_DEFAULT_REPO:=backlog}"
  : "${FORGEJO_AGENT_NAME:=codex}"
  : "${FORGEJO_LEASE_MINUTES:=90}"
  : "${FORGEJO_BLOCKED_LABEL:=state/blocked}"
  : "${FORGEJO_READY_LABEL:=state/ready}"

  local token_file="${FORGEJO_TOKEN_FILE:-$FORGEJO_AGENT_TOKEN_FILE_DEFAULT}"
  token_file="$(expand_tilde "$token_file")"
  [[ -f "$token_file" ]] || err "missing token file: $token_file"

  FORGEJO_TOKEN="$(tr -d '\r\n' < "$token_file")"
  [[ -n "$FORGEJO_TOKEN" ]] || err "token file is empty: $token_file"

  export FORGEJO_BASE_URL
  export FORGEJO_DEFAULT_OWNER
  export FORGEJO_DEFAULT_REPO
  export FORGEJO_AGENT_NAME
  export FORGEJO_LEASE_MINUTES
  export FORGEJO_BLOCKED_LABEL
  export FORGEJO_READY_LABEL
  export FORGEJO_TOKEN
}

forgejo_api() {
  local method="$1"
  local path="$2"
  local data="${3-}"
  local url="${FORGEJO_BASE_URL%/}${path}"

  local -a args=(
    -fsS
    -X "$method"
    "$url"
    -H "Accept: application/json"
    -H "Authorization: token $FORGEJO_TOKEN"
  )

  if [[ -n "$data" ]]; then
    args+=(
      -H "Content-Type: application/json"
      --data "$data"
    )
  fi

  curl "${args[@]}"
}

parse_repo_ref() {
  local ref="$1"
  if [[ "$ref" =~ ^([^/]+)/([^#]+)#([0-9]+)$ ]]; then
    REPO_OWNER="${BASH_REMATCH[1]}"
    REPO_NAME="${BASH_REMATCH[2]}"
    ISSUE_NUMBER="${BASH_REMATCH[3]}"
    return
  fi
  err "expected ref like owner/repo#123, got: $ref"
}

parse_repo_only() {
  local ref="$1"
  if [[ "$ref" =~ ^([^/]+)/([^/]+)$ ]]; then
    REPO_OWNER="${BASH_REMATCH[1]}"
    REPO_NAME="${BASH_REMATCH[2]}"
    return
  fi
  err "expected repo like owner/repo, got: $ref"
}

ensure_label_id() {
  local owner="$1"
  local repo="$2"
  local name="$3"
  local color="$4"
  local description="$5"

  local label_id
  label_id="$(forgejo_api GET "/api/v1/repos/$owner/$repo/labels?limit=1000" | jq -r --arg n "$name" '.[] | select(.name == $n) | .id' | head -n1)"

  if [[ -z "$label_id" ]]; then
    label_id="$(forgejo_api POST "/api/v1/repos/$owner/$repo/labels" "$(jq -cn --arg n "$name" --arg c "$color" --arg d "$description" '{name:$n,color:$c,description:$d}')" | jq -r '.id')"
  fi

  printf '%s\n' "$label_id"
}

issue_json() {
  local owner="$1"
  local repo="$2"
  local number="$3"
  forgejo_api GET "/api/v1/repos/$owner/$repo/issues/$number"
}

repo_from_arg_or_default() {
  local arg="${1-}"
  if [[ -z "$arg" ]]; then
    REPO_OWNER="$FORGEJO_DEFAULT_OWNER"
    REPO_NAME="$FORGEJO_DEFAULT_REPO"
  else
    parse_repo_only "$arg"
  fi
}

iso_now_utc() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

iso_plus_minutes_utc() {
  local minutes="$1"
  date -u -d "+${minutes} minutes" +"%Y-%m-%dT%H:%M:%SZ"
}
