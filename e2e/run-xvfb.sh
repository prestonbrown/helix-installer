#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Runs e2e/run-update.mjs against the debug app on a private Xvfb display, so no window reaches
# the desktop. Needs tauri-driver (cargo install tauri-driver) and WebKitWebDriver (the
# webkit2gtk-driver package; WEBKIT_DRIVER names it when it is not on PATH). The E2E_* variables
# run-update.mjs reads pass through; the app's own HELIX_INSTALLER_* variables do too.
#
#   E2E_LOG   where the driver's and the app's output goes (default e2e-app.log)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(dirname "$here")
export E2E_APP=${E2E_APP:-$root/target/debug/helix_installer_app}
port=${E2E_PORT:-4444}
log=${E2E_LOG:-e2e-app.log}
driver=${WEBKIT_DRIVER:-$(command -v WebKitWebDriver || true)}
[ -x "$driver" ] || { echo "WebKitWebDriver not found: install webkit2gtk-driver or set WEBKIT_DRIVER" >&2; exit 1; }
[ -x "$E2E_APP" ] || { echo "$E2E_APP is not built: cargo tauri build --debug --no-bundle" >&2; exit 1; }

display=:$((90 + RANDOM % 100))
Xvfb "$display" -screen 0 1280x800x24 -nolisten tcp >/dev/null 2>&1 &
xvfb=$!
pids=("$xvfb")
cleanup() { kill "${pids[@]}" 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

env -u WAYLAND_DISPLAY DISPLAY="$display" GDK_BACKEND=x11 \
  tauri-driver --port "$port" --native-port $((port + 1)) --native-driver "$driver" >"$log" 2>&1 &
pids+=("$!")

for _ in $(seq 50); do
  curl -sf "http://127.0.0.1:$port/status" >/dev/null 2>&1 && break
  sleep 0.2
done

E2E_DRIVER="http://127.0.0.1:$port" node "$here/run-update.mjs"
