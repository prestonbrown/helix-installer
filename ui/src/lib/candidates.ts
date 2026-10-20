// SPDX-License-Identifier: GPL-3.0-or-later
// What the picker says about each printer it found, by how it is reached and what it runs.
import { sentence } from "./text";
import type { CandidateView, PrinterView } from "./types";

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

/** A MAC address, or its last three bytes, glued to the end of a host name by an image that
 * makes names unique (`voron-b827eb1a2b3c`, `k1-dc:a6:32:01:02:03`, `mainsailos-1a2b3c`). Hex
 * that is all digits or all letters reads as part of the name and stays. */
const MAC_SUFFIX = /[-_](?:[0-9a-f]{2}[:-]?){5}[0-9a-f]{2}$|[-_][0-9a-f]{6}$/i;

function hostTitle(host: string | null): string | null {
  const h = host?.trim();
  if (!h) return null;
  const m = h.match(MAC_SUFFIX);
  if (!m) return h;
  const hex = m[0].slice(1).replace(/[:-]/g, "");
  const plain = hex.length === 12 || (/[0-9]/.test(hex) && /[a-f]/i.test(hex));
  const rest = h.slice(0, m.index);
  return plain && rest ? rest : h;
}

/** The models an ambiguous printer could be, sharing their common leading words:
 * "FlashForge Adventurer 5M or 5M Pro". */
function modelList(choices: PrinterView[]): string {
  const words = choices.map((p) => p.name.split(" "));
  let common = 0;
  while (words.every((w) => w.length > common + 1 && w[common] === words[0][common])) common++;
  const names = words.map((w, i) => (i === 0 ? w : w.slice(common)).join(" "));
  return names.length <= 1
    ? names.join("")
    : `${names.slice(0, -1).join(", ")} or ${names[names.length - 1]}`;
}

/** The card's heading: the printer's own name when it gives one, else what the installer
 * takes it for. A printer that could be several models is named by those models, since its
 * host name is often a model id (`ad5m`) that says nothing to the user. */
export function cardTitle(c: CandidateView): string {
  if (c.status === "choose" && c.choices.length > 0) return modelList(c.choices);
  return nameLines(c.name, c.host_name).title;
}

/** What the installer takes the printer for, under a title that is the printer's own name. */
export function cardSubtitle(c: CandidateView): string | null {
  if (c.status === "choose" && c.choices.length > 0) return null;
  return nameLines(c.name, c.host_name).subtitle;
}

/** A printer's name for a heading: its own host name over the installer's name for it, or
 * that name alone when it gave none. Picker cards and the run's header both use it. */
export function nameLines(displayName: string, hostName: string | null): { title: string; subtitle: string | null } {
  const own = hostTitle(hostName);
  return own === null ? { title: displayName, subtitle: null } : { title: own, subtitle: displayName };
}

/** What a card says about a printer the installer turns away: that it can't, and why. */
export function unsupportedLines(c: CandidateView): string[] {
  const lines = ["The installer can't set up this printer. Its guide shows how to install by hand."];
  if (c.reason) lines.push(sentence(c.reason));
  return lines;
}

/** The models the user can type an address for: every printer reached over the network,
 * refused ones included, since picking one shows its refusal and guide. A printer found only
 * by USB is pointed at the cable instead. */
export function manualModels(printers: PrinterView[]): { models: PrinterView[]; usbLines: string[] } {
  return {
    models: printers.filter((p) => !p.usb_only),
    usbLines: printers.filter((p) => p.usb_only).map((p) => `${p.name}? Plug it in by USB instead.`),
  };
}
