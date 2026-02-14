#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERVICE_SRC="$ROOT_DIR/templates/orchd.service"
SERVICE_DIR="$HOME/.config/systemd/user"
SERVICE_DST="$SERVICE_DIR/orchd.service"

if [[ ! -f "$SERVICE_SRC" ]]; then
  echo "missing service template: $SERVICE_SRC" >&2
  exit 1
fi

mkdir -p "$SERVICE_DIR"
install -m 0644 "$SERVICE_SRC" "$SERVICE_DST"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=${XDG_RUNTIME_DIR}/bus}"

# Prevent bind conflicts if an old manually-launched orchd is still running.
pkill -TERM -f '^/home/main/forgejo-agent/target/debug/orchd ' || true
pkill -TERM -f '^/home/main/.local/bin/orchd ' || true
sleep 1

systemctl --user daemon-reload
systemctl --user enable --now orchd.service
systemctl --user restart orchd.service

echo
echo "orchd.service status:"
systemctl --user --no-pager --full status orchd.service | sed -n '1,40p'
echo
echo "tail logs with:"
echo "  XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS journalctl --user -u orchd.service -f"
