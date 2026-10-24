#!/bin/sh
# Start a headless labwc (one virtual output, software rendering) for testing
# the native capture backends on any machine, e.g. inside a GNOME session.
# Prints the new WAYLAND_DISPLAY; labwc runs in the background.
#
# Usage: tools/labwc-headless.sh [LOGFILE [SECONDS]]
#   LOGFILE  labwc's output (default /dev/null)
#   SECONDS  stop labwc after this long (default 3600)
#
# Example:
#   WD=$(tools/labwc-headless.sh)
#   WAYLAND_DISPLAY=$WD weston-simple-shm &   # something that animates
#   WAYLAND_DISPLAY=$WD wlcast
set -eu
runtime=${XDG_RUNTIME_DIR:?}
sockets() { ls "$runtime" | grep -E '^wayland-[0-9]+$' | sort || true; }

before=$(sockets)
env -u WAYLAND_DISPLAY -u DISPLAY \
  WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 \
  WLR_LIBINPUT_NO_DEVICES=1 \
  timeout "${2:-3600}" labwc > "${1:-/dev/null}" 2>&1 &

for _ in 1 2 3 4 5 6 7 8 9 10; do
  sleep 0.5
  new=$(sockets | grep -vxF "$before" | head -n 1 || true)
  if [ -n "$new" ]; then
    echo "$new"
    exit 0
  fi
done
echo "labwc didn't start" >&2
exit 1
