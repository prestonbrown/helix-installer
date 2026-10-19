// SPDX-License-Identifier: GPL-3.0-or-later
// What the picker says about each printer it found, by how it is reached and what it runs.
import type { CandidateView } from "./types";

/** Where the candidate is: its network address, or the USB port a screen is plugged into. */
export function whereLine(c: CandidateView): string {
  return c.connection === "usb" ? `Connected by USB (${c.address})` : `At ${c.address}`;
}

/** The status of a candidate ready to start. A screen on USB is only read once its install
 * starts, since opening its port restarts it, so what it runs is not known yet. */
export function readyLine(c: CandidateView): string {
  if (c.helix_version) return `HelixScreen ${c.helix_version} is installed`;
  if (c.helix_version === "") return "HelixScreen is installed";
  if (c.connection === "usb") return "The installer checks what it runs before changing anything";
  return "Ready to install";
}

/** The button that starts a ready candidate. */
export function startLabel(c: CandidateView): string {
  if (c.connection === "usb" && c.helix_version === null) return "Install or update HelixScreen";
  return c.helix_version !== null ? "Update HelixScreen" : "Install HelixScreen";
}
