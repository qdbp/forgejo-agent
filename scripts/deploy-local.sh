#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SERVICE_FILE="${ORCHD_SERVICE_FILE:-$HOME/.config/systemd/user/orchd.service}"
FORGEJOCTL_BIN="${FORGEJOCTL_BIN:-$HOME/.local/bin/forgejoctl}"
ORCHD_BIN="${ORCHD_BIN:-$HOME/.local/bin/orchd}"

snapshot_dir=""
rollback_needed=0
rolling_back=0

backup_binary() {
  local path="$1"
  local key="$2"
  local state_file="$snapshot_dir/${key}.state"
  local backup_file="$snapshot_dir/${key}.bak"

  if [[ -f "$path" ]]; then
    cp -a "$path" "$backup_file"
    echo "present" >"$state_file"
  else
    echo "absent" >"$state_file"
  fi
}

restore_binary() {
  local path="$1"
  local key="$2"
  local state_file="$snapshot_dir/${key}.state"
  local backup_file="$snapshot_dir/${key}.bak"
  local state=""
  if [[ -f "$state_file" ]]; then
    state="$(cat "$state_file")"
  fi

  if [[ "$state" == "present" ]]; then
    install -m 0755 "$backup_file" "$path"
    return
  fi

  rm -f "$path"
}

ensure_systemd_user_env() {
  local uid
  uid="$(id -u)"
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$uid}"
  export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=${XDG_RUNTIME_DIR}/bus}"
  if [[ -d "$XDG_RUNTIME_DIR" ]]; then
    return
  fi
  echo "[deploy-local] missing XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR" >&2
  return 1
}

restart_orchd_service() {
  systemctl --user daemon-reload
  systemctl --user restart orchd.service
  systemctl --user --quiet is-active orchd.service
}

rollback_install() {
  if (( rollback_needed == 0 || rolling_back == 1 )); then
    return
  fi
  rolling_back=1
  trap - ERR

  echo "[deploy-local] rollback: restoring previous binaries" >&2
  restore_binary "$FORGEJOCTL_BIN" "forgejoctl"
  restore_binary "$ORCHD_BIN" "orchd"

  if [[ -f "$SERVICE_FILE" ]]; then
    if ensure_systemd_user_env && restart_orchd_service; then
      echo "[deploy-local] rollback: orchd.service restored" >&2
    else
      echo "[deploy-local] rollback: orchd.service restore failed" >&2
    fi
  fi
}

cleanup() {
  if [[ -n "$snapshot_dir" ]]; then
    rm -rf "$snapshot_dir"
  fi
}

trap cleanup EXIT
trap rollback_install ERR

snapshot_dir="$(mktemp -d)"
backup_binary "$FORGEJOCTL_BIN" "forgejoctl"
backup_binary "$ORCHD_BIN" "orchd"
rollback_needed=1

echo "[deploy-local] building + installing binaries"
"$ROOT_DIR/scripts/install.sh" "$FORGEJOCTL_BIN" "$ORCHD_BIN"

if [[ -f "$SERVICE_FILE" ]]; then
  ensure_systemd_user_env
  echo "[deploy-local] restarting orchd.service"
  restart_orchd_service
  echo "[deploy-local] orchd.service is active"
else
  echo "[deploy-local] skip orchd restart (missing service file: $SERVICE_FILE)"
fi

rollback_needed=0
