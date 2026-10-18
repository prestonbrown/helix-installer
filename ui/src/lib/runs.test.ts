// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { resumeOrder } from "./runs";
import type { RunSummary } from "./types";

const run = (run_id: string, committed: boolean): RunSummary => ({
  run_id,
  printer_name: run_id,
  address: "192.168.1.50",
  step: 1,
  total_steps: 4,
  committed,
  reason: null,
});

describe("resumeOrder", () => {
  it("lists every run, the ones that changed a printer first", () => {
    const runs = [run("run-1-0", true), run("run-2-0", false), run("run-3-0", true), run("run-4-0", false)];
    expect(resumeOrder(runs).map((r) => r.run_id)).toEqual(["run-3-0", "run-1-0", "run-4-0", "run-2-0"]);
  });
  it("keeps a committed run ahead of a newer one that changed nothing", () => {
    const runs = [run("run-1-0", true), run("run-9-0", false)];
    expect(resumeOrder(runs)[0].run_id).toBe("run-1-0");
  });
  it("leaves its input alone", () => {
    const runs = [run("run-1-0", false), run("run-2-0", true)];
    resumeOrder(runs);
    expect(runs.map((r) => r.run_id)).toEqual(["run-1-0", "run-2-0"]);
  });
});
