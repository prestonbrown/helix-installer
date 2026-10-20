// SPDX-License-Identifier: GPL-3.0-or-later
// Development only: `vite --mode mock` resolves `@tauri-apps/api/core` to this file, so every
// command reaches the mock's handler, in a plain browser and in the app's own webview alike. That
// webview defines its IPC internals read-only, so they cannot be mocked in place.
type Handler = (cmd: string, args?: Record<string, unknown>) => unknown;

let handler: Handler = (cmd) => {
  throw { kind: "unexpected", message: `mock has no ${cmd}`, doc: null };
};

export function setHandler(h: Handler): void {
  handler = h;
}

export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return (await handler(cmd, args)) as T;
}
