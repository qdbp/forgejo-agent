#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "[deploy-local] building + installing binaries"
"$ROOT_DIR/scripts/install.sh"

# Best-effort local hot-reload for developer UX.
SERVICE_FILE="$HOME/.config/systemd/user/orchd.service"
if [[ -f "$SERVICE_FILE" ]]; then
  uid="$(id -u)"
  export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$uid}"
  export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=${XDG_RUNTIME_DIR}/bus}"

  if [[ -d "$XDG_RUNTIME_DIR" ]]; then
    echo "[deploy-local] restarting orchd.service"
    systemctl --user daemon-reload || true
    systemctl --user restart orchd.service || true
  else
    echo "[deploy-local] skip orchd.service restart (missing XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR)" >&2
  fi
fi

