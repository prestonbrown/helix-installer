// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { keepOnLabel, keepPowerOn, keepPowerText, retryAdvice, stepLabel, stoppedState } from "./text";

describe("a K-Touch flashed over USB", () => {
  it("names the step", () => {
    expect(stepLabel("serial_flash")).toBe("Writing HelixScreen to the screen");
  });
  it("must stay plugged in while it is written, not kept powered like a printer", () => {
    expect(keepPowerOn("serial_flash", false)).toBe(true);
    expect(keepOnLabel("serial_flash")).toBe("Do not unplug the K-Touch");
    expect(keepPowerText("serial_flash")).toMatch(/^Do not unplug the K-Touch\./);
    expect(retryAdvice("serial_flash")).toMatch(/plugged into this computer/);
  });
  it("says a half-written screen may not start and cannot be damaged", () => {
    const s = stoppedState("serial_flash");
    expect(s).toMatch(/may not start/);
    expect(s).toMatch(/cannot damage/);
  });
  it("leaves the printer wording for every other step", () => {
    expect(keepOnLabel("helix_install")).toBe("Do not turn off your printer");
    expect(keepPowerText(null)).toBe("Do not turn off your printer. This can take several minutes.");
    expect(retryAdvice("ssh_run")).toMatch(/switched on and connected to your network/);
  });
});
