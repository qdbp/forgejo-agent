#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORCHD_SERVICE_SRC="$ROOT_DIR/templates/orchd.service"
SCHEDULE_SERVICE_SRC="$ROOT_DIR/templates/orchd-schedule-tick.service"
SCHEDULE_TIMER_SRC="$ROOT_DIR/templates/orchd-schedule-tick.timer"
SERVICE_DIR="$HOME/.config/systemd/user"
ORCHD_SERVICE_DST="$SERVICE_DIR/orchd.service"
SCHEDULE_SERVICE_DST="$SERVICE_DIR/orchd-schedule-tick.service"
SCHEDULE_TIMER_DST="$SERVICE_DIR/orchd-schedule-tick.timer"
ORCHD_TOKEN_FILE="$HOME/.config/forgejo-agent/creds/orchd.token"

if [[ ! -r "$ORCHD_TOKEN_FILE" ]]; then
  echo "missing orchd token file: $ORCHD_TOKEN_FILE" >&2
  echo "create an orchd Forgejo token and store it there before installing the service." >&2
  exit 1
fi

for template in "$ORCHD_SERVICE_SRC" "$SCHEDULE_SERVICE_SRC" "$SCHEDULE_TIMER_SRC"; do
  if [[ ! -f "$template" ]]; then
    echo "missing service template: $template" >&2
    exit 1
  fi
done

echo "building + installing orchd + forgejoctl..."
"$ROOT_DIR/scripts/install.sh"

mkdir -p "$SERVICE_DIR"
install -m 0644 "$ORCHD_SERVICE_SRC" "$ORCHD_SERVICE_DST"
install -m 0644 "$SCHEDULE_SERVICE_SRC" "$SCHEDULE_SERVICE_DST"
install -m 0644 "$SCHEDULE_TIMER_SRC" "$SCHEDULE_TIMER_DST"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=${XDG_RUNTIME_DIR}/bus}"

# Prevent bind conflicts if an old manually-launched orchd is still running.
pkill -TERM -f '^/home/main/forgejo-agent/target/debug/orchd ' || true
pkill -TERM -f '^/home/main/.local/bin/orchd ' || true
sleep 1

systemctl --user daemon-reload
systemctl --user enable --now orchd.service orchd-schedule-tick.timer
systemctl --user restart orchd.service
systemctl --user restart orchd-schedule-tick.timer

echo
echo "orchd.service status:"
systemctl --user --no-pager --full status orchd.service | sed -n '1,40p'
echo
echo "orchd-schedule-tick.timer status:"
systemctl --user --no-pager --full status orchd-schedule-tick.timer | sed -n '1,40p'
echo
echo "tail logs with:"
echo "  XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS journalctl --user -u orchd.service -f"
