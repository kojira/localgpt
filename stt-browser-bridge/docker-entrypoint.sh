#!/bin/sh
set -e

# PulseAudio: user daemon (bridge user) so pactl/paplay work
echo "[entrypoint] Starting PulseAudio..."
pulseaudio -D --exit-idle-time=-1 --log-target=stderr 2>&1 || {
  echo "[entrypoint] PulseAudio daemon failed, trying --start..."
  pulseaudio --start --exit-idle-time=-1 2>&1 || true
}
sleep 1

# Verify PulseAudio is running and export PULSE_SERVER for Chromium
if pactl info >/dev/null 2>&1; then
  echo "[entrypoint] PulseAudio is running"
  PULSE_SOCKET=$(ls /tmp/pulse-*/native 2>/dev/null | head -1)
  if [ -n "$PULSE_SOCKET" ]; then
    export PULSE_SERVER="unix:$PULSE_SOCKET"
    echo "[entrypoint] PULSE_SERVER=$PULSE_SERVER"
  fi
else
  echo "[entrypoint] WARNING: PulseAudio not running"
fi

# Xvfb for Puppeteer (headless: false for Web Speech API)
echo "[entrypoint] Starting Xvfb..."
Xvfb :99 -screen 0 1280x1024x24 &
export DISPLAY=:99
sleep 1

# Note: null-sink is created by server.js (virtual-device.js).
# After it starts, we need the default source set to the monitor.
# server.js calls virtual-device.create() which creates the sink,
# so we set default source in virtual-device.js instead.
echo "[entrypoint] Starting server..."
exec node server.js
