#!/bin/bash
# Keep a reverse SSH tunnel NYC:10000 → Mac:9090 alive.
# Every 30 seconds: if /health through the tunnel fails, restart the tunnel.
set -u
LOG=/tmp/arc-tunnel-watchdog.log
TUNNEL_HOST=root@149.28.32.76
REMOTE_PORT=10000
LOCAL_PORT=9090

echo "[$(date)] watchdog starting" >> "$LOG"

while true; do
  if ! curl -s -m 5 -o /dev/null -w "%{http_code}\n" "http://149.28.32.76:${REMOTE_PORT}/health" 2>/dev/null | grep -q 200; then
    echo "[$(date)] tunnel down, reconnecting" >> "$LOG"
    pkill -f "ssh.*-R.*${REMOTE_PORT}" 2>/dev/null || true
    sleep 1
    ssh -N -f \
      -o ExitOnForwardFailure=yes \
      -o ServerAliveInterval=30 \
      -o ServerAliveCountMax=3 \
      -o StrictHostKeyChecking=no \
      -R "0.0.0.0:${REMOTE_PORT}:127.0.0.1:${LOCAL_PORT}" \
      "$TUNNEL_HOST" 2>>"$LOG"
  fi
  sleep 30
done
