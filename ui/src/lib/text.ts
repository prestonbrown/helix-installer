// SPDX-License-Identifier: GPL-3.0-or-later
// Wording the screens share, keyed by the manifest step type the engine reports.

const STEP_LABELS: Record<string, string> = {
  info: "Getting ready",
  user_action: "A step on your printer",
  ask: "A question about your printer",
  probe: "Checking your printer",
  require: "Checking your printer's software version",
  download: "Downloading files",
  write_stick: "Copying files to the USB stick",
  wait_for: "Waiting for your printer",
  ssh_login: "Connecting to your printer",
  ssh_run: "Setting up your printer",
  moonraker_gcode: "Switching your printer's screen",
  moonraker_update: "Updating HelixScreen",
  helix_install: "Installing HelixScreen",
  serial_flash: "Writing HelixScreen to the screen",

};

export function stepLabel(kind: string | null): string {
  return (kind && STEP_LABELS[kind]) || "Installing";
}

/** Steps during which the printer is busy changing itself and must keep its power. */
const PRINTER_BUSY = new Set([
  "wait_for",
  "ssh_run",
  "helix_install",
  "moonraker_gcode",
  "moonraker_update",
  "serial_flash",
]);

export function keepPowerOn(kind: string | null, committed: boolean): boolean {
  return committed || (kind !== null && PRINTER_BUSY.has(kind));
}

/** What must not happen while `kind` runs: a screen flashed over USB must stay plugged in. */
export function keepOnLabel(kind: string | null): string {
  return kind === "serial_flash" ? "Do not unplug the K-Touch" : "Do not turn off your printer";
}

export function keepPowerText(kind: string | null): string {
  const takes = kind === "serial_flash" ? "a few minutes" : "several minutes";
  return `${keepOnLabel(kind)}. This can take ${takes}.`;
}

/** How to leave the printer before trying a stopped step again. */
export function retryAdvice(kind: string | null): string {
  return kind === "serial_flash"
    ? "Keep the K-Touch plugged into this computer"
    : "Leave the printer switched on and connected to your network";
}

/** What the printer is in now, after the install stopped part-way through `kind`. */
export function stoppedState(kind: string | null): string {
  switch (kind) {
    case "serial_flash":
      return "HelixScreen was being written to the K-Touch. It may not start, or its screen may stay blank, until it is written again. This cannot damage it: its chip always accepts a new write over USB.";
    case "helix_install":
      return "HelixScreen is only partly installed. The printer's screen may be blank, or may still show the old screen, until the install finishes.";
    case "ssh_run":
      return "The printer's software is partly changed. The printer may restart or show an unusual screen until this step finishes.";
    case "wait_for":
      return "The printer has been changed and the installer is waiting for it to start up again. Some printers take a long time to start after an update.";
    case "moonraker_gcode":
    case "moonraker_update":
      return "The installer asked your printer to switch to HelixScreen, and the switch did not finish. The printer's screen may be blank or restarting.";
    default:
      return "The installer has already changed your printer, and it stopped before the end. The printer may not work normally until the install is finished.";
  }
}

export function isUrl(s: string | null | undefined): s is string {
  return !!s && /^https?:\/\//.test(s);
}

/** A manifest image path made absolute against where the printer list came from. */
export function imageUrl(src: string | null | undefined, base: string | null): string | null {
  if (!src) return null;
  if (/^[a-z][a-z0-9+.-]*:/i.test(src)) return src;
  if (!base) return src;
  try {
    return new URL(src, base).toString();
  } catch {
    return null;
  }
}

export function formatBytes(n: number): string {
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i++;
  }
  return i === 0 ? `${v} ${units[0]}` : `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}
