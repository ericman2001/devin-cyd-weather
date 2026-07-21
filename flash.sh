#!/usr/bin/env bash
#
# Build, flash and monitor the CYD Weather Station firmware.
#
# The Cheap Yellow Display (ESP32-2432S028R) exposes a CH340 USB-serial bridge
# on its *micro-USB* port. Connect the board via that data port (not a
# charge-only cable / the USB-C power port on 2-USB variants) before running.
#
# Usage:
#   ./flash.sh                # auto-detect the serial port, build + flash + monitor
#   ./flash.sh /dev/ttyUSB0   # use an explicit port
#   BAUD=460800 ./flash.sh    # override the flashing baud rate

set -euo pipefail

BAUD="${BAUD:-460800}"
PORT="${1:-}"

# Make sure the ESP Rust toolchain env is available (espup writes this file).
if [[ -f "$HOME/export-esp.sh" ]]; then
  # shellcheck disable=SC1091
  source "$HOME/export-esp.sh"
fi

if ! command -v espflash >/dev/null 2>&1; then
  echo "error: 'espflash' not found. Install it with: cargo install espflash" >&2
  exit 1
fi

# Auto-detect a likely CH340 serial port if none was supplied.
if [[ -z "$PORT" ]]; then
  for candidate in /dev/ttyUSB* /dev/ttyACM* /dev/tty.usbserial-* /dev/tty.wchusbserial*; do
    if [[ -e "$candidate" ]]; then
      PORT="$candidate"
      break
    fi
  done
fi

if [[ -z "$PORT" ]]; then
  echo "warning: could not auto-detect a serial port; letting espflash choose." >&2
  PORT_ARG=()
else
  echo "Using serial port: $PORT"
  PORT_ARG=(--port "$PORT")
fi

echo "Building (release) and flashing at ${BAUD} baud..."
exec cargo espflash flash \
  --release \
  --baud "$BAUD" \
  "${PORT_ARG[@]}" \
  --monitor
