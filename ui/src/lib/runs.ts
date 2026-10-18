// SPDX-License-Identifier: GPL-3.0-or-later
import type { RunSummary } from "./types";

/** Unfinished runs in the order the resume banner offers them: runs that already changed a
 * printer first, since that printer may not work until the install finishes; newest first
 * within each group (run ids sort by start time). */
export function resumeOrder(runs: RunSummary[]): RunSummary[] {
  return [...runs].sort(
    (a, b) => Number(b.committed) - Number(a.committed) || b.run_id.localeCompare(a.run_id),
  );
}
