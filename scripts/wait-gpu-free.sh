#!/usr/bin/env bash
# wait-gpu-free.sh — block until no compute processes hold the P100s, or timeout.
# Used by nightdrive-album-drop.service between ACE-Step shutdown and openclaw-inference restart
# to dodge the restart-loop hazard from leftover CUDA contexts.

set -e

TIMEOUT_SECS="${1:-60}"
POLL_SECS=5
RETRIES=$(( TIMEOUT_SECS / POLL_SECS ))

for i in $(seq 1 "$RETRIES"); do
  # Query compute apps. Empty output (after header) = no processes holding GPUs.
  busy=$(nvidia-smi --query-compute-apps=pid --format=csv,noheader 2>/dev/null | grep -v '^$' || true)
  if [ -z "$busy" ]; then
    echo "wait-gpu-free: GPUs idle after ${i} polls"
    exit 0
  fi
  echo "wait-gpu-free: GPUs still busy (try ${i}/${RETRIES}): $(echo "$busy" | tr '\n' ',' | sed 's/,$//')"
  sleep "$POLL_SECS"
done

echo "wait-gpu-free: TIMEOUT after ${TIMEOUT_SECS}s; proceeding with restart anyway" >&2
exit 0   # exit 0 so systemd ExecStopPost doesn't fail — best-effort wait
