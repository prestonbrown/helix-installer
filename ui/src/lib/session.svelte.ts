// SPDX-License-Identifier: GPL-3.0-or-later
// App-wide values every screen may read.
import type { PrinterView } from "./types";

export const session = $state({
  version: "",
  imageBase: null as string | null,
  printers: [] as PrinterView[],
  /** Revealed by clicking the version label five times. */
  recordMode: false,
});

export function printerName(id: string): string {
  return session.printers.find((p) => p.id === id)?.name ?? id;
}
