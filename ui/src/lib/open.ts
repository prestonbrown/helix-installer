// SPDX-License-Identifier: GPL-3.0-or-later
// Guides open in the user's own browser, never inside the installer window.
import { openUrl } from "@tauri-apps/plugin-opener";

export async function openGuide(url: string): Promise<void> {
  try {
    await openUrl(url);
  } catch {
    window.open(url, "_blank", "noopener");
  }
}
