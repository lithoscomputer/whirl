#!/usr/bin/env bash
# Run a command, printing its output only when it fails.
# Set WHIRL_VERBOSE=1 to stream output unconditionally.
set -u

if [ "${WHIRL_VERBOSE:-0}" = "1" ]; then
  exec "$@"
fi

out=$(mktemp)
trap 'rm -f "$out"' EXIT

"$@" >"$out" 2>&1
code=$?
if [ "$code" -ne 0 ]; then
  cat "$out"
fi
exit "$code"
