#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SERVICE_FILE="${ORCHD_SERVICE_FILE:-$HOME/.config/systemd/user/orchd.service}"
SERVICE_DIR="${ORCHD_SERVICE_DIR:-$(dirname "$SERVICE_FILE")}"
SCHEDULE_SERVICE_FILE="${ORCHD_SCHEDULE_SERVICE_FILE:-$SERVICE_DIR/orchd-schedule-tick.service}"
SCHEDULE_TIMER_FILE="${ORCHD_SCHEDULE_TIMER_FILE:-$SERVICE_DIR/orchd-schedule-tick.timer}"
DEPLOYD_SERVICE_FILE="${ORCHD_DEPLOYD_SERVICE_FILE:-$SERVICE_DIR/orchd-deployd.service}"
FORGEJOCTL_BIN="${FORGEJOCTL_BIN:-$HOME/.local/bin/forgejoctl}"
ORCHD_BIN="${ORCHD_BIN:-$HOME/.local/bin/orchd}"
ORCHD_SERVICE_TEMPLATE="${ORCHD_SERVICE_TEMPLATE:-$ROOT_DIR/templates/orchd.service}"
DEPLOYD_SERVICE_TEMPLATE="${ORCHD_DEPLOYD_SERVICE_TEMPLATE:-$ROOT_DIR/templates/orchd-deployd.service}"
SCHEDULE_SERVICE_TEMPLATE="${ORCHD_SCHEDULE_SERVICE_TEMPLATE:-$ROOT_DIR/templates/orchd-schedule-tick.service}"
SCHEDULE_TIMER_TEMPLATE="${ORCHD_SCHEDULE_TIMER_TEMPLATE:-$ROOT_DIR/templates/orchd-schedule-tick.timer}"
SKIP_ORCHD_RESTART="${ORCHD_DEPLOY_SKIP_ORCHD_RESTART:-0}"
SKIP_TIMER_RESTART="${ORCHD_DEPLOY_SKIP_TIMER_RESTART:-0}"
SKIP_DEPLOYD_RESTART="${ORCHD_DEPLOY_SKIP_DEPLOYD_RESTART:-0}"

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
  systemctl --user restart orchd.service
  systemctl --user --quiet is-active orchd.service
}

restart_deployd_service() {
  systemctl --user restart orchd-deployd.service
  systemctl --user --quiet is-active orchd-deployd.service
}

install_orchd_units() {
  mkdir -p "$SERVICE_DIR"
  install -m 0644 "$ORCHD_SERVICE_TEMPLATE" "$SERVICE_FILE"
  install -m 0644 "$DEPLOYD_SERVICE_TEMPLATE" "$DEPLOYD_SERVICE_FILE"
  install -m 0644 "$SCHEDULE_SERVICE_TEMPLATE" "$SCHEDULE_SERVICE_FILE"
  install -m 0644 "$SCHEDULE_TIMER_TEMPLATE" "$SCHEDULE_TIMER_FILE"
}

restart_schedule_timer() {
  systemctl --user enable --now orchd-schedule-tick.timer
  systemctl --user restart orchd-schedule-tick.timer
  systemctl --user --quiet is-active orchd-schedule-tick.timer
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
    if [[ -f "$DEPLOYD_SERVICE_FILE" ]]; then
      if restart_deployd_service; then
        echo "[deploy-local] rollback: orchd-deployd.service restored" >&2
      else
        echo "[deploy-local] rollback: orchd-deployd.service restore failed" >&2
      fi
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
  install_orchd_units
  systemctl --user daemon-reload
  systemctl --user enable --now orchd.service orchd-deployd.service
  if [[ "$SKIP_ORCHD_RESTART" == "1" ]]; then
    echo "[deploy-local] skipping orchd.service restart (ORCHD_DEPLOY_SKIP_ORCHD_RESTART=1)"
  else
    echo "[deploy-local] restarting orchd.service"
    restart_orchd_service
    echo "[deploy-local] orchd.service is active"
  fi
  if [[ "$SKIP_DEPLOYD_RESTART" == "1" ]]; then
    echo "[deploy-local] skipping orchd-deployd.service restart (ORCHD_DEPLOY_SKIP_DEPLOYD_RESTART=1)"
  else
    echo "[deploy-local] restarting orchd-deployd.service"
    restart_deployd_service
    echo "[deploy-local] orchd-deployd.service is active"
  fi
  if [[ "$SKIP_TIMER_RESTART" == "1" ]]; then
    echo "[deploy-local] skipping orchd-schedule-tick.timer restart (ORCHD_DEPLOY_SKIP_TIMER_RESTART=1)"
  else
    echo "[deploy-local] restarting orchd-schedule-tick.timer"
    restart_schedule_timer
  fi
else
  echo "[deploy-local] skip orchd restart (missing service file: $SERVICE_FILE)"
fi

rollback_needed=0
