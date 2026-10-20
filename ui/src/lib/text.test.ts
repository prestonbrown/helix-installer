// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { finishScreen, keepOnLabel, keepPowerText, retryAdvice, stepLabel, stoppedState } from "./text";

describe("a K-Touch flashed over USB", () => {
  it("names the step", () => {
    expect(stepLabel("serial_flash", true)).toBe("Writing HelixScreen to the screen");
  });
  it("must stay plugged in while it is written, not kept powered like a printer", () => {
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

describe("before the point of no return", () => {
  it("calls the install step a check while nothing has changed", () => {
    expect(stepLabel("helix_install", false)).toBe("Checking your printer");
    expect(stepLabel("helix_install", true)).toBe("Installing HelixScreen");
  });
  it("labels every other step the same either side of it", () => {
    expect(stepLabel("ssh_login", false)).toBe("Connecting to your printer");
    expect(stepLabel("ssh_login", true)).toBe("Connecting to your printer");
    expect(stepLabel(null, false)).toBe("Installing");
  });
});

describe("the finish screen", () => {
  it("reads an up-to-date check as a success that changed nothing", () => {
    const s = finishScreen(
      "up_to_date",
      "1.1.0-beta.10 is installed, and the newest beta or stable release is 1.1.0-beta.10",
      "Klipper host",
    );
    expect(s.heading).toBe("HelixScreen is up to date");
    expect(s.lines).toEqual([
      "1.1.0-beta.10 is installed, and the newest beta or stable release is 1.1.0-beta.10.",
      "Nothing on your printer was changed.",
    ]);
  });
  it("says HelixScreen is installed after a real install", () => {
    const s = finishScreen(null, null, "Creality K1");
    expect(s.heading).toBe("HelixScreen is installed");
    expect(s.lines[0]).toMatch(/Your Creality K1 may restart/);
  });
  it("shows the note of an install that ended with something to check, never that nothing changed", () => {
    const s = finishScreen(
      "note",
      "HelixScreen 1.1.0 was written to the BTT K-Touch and checked, and it restarted, but the installer could not read its version back: check its screen shows HelixScreen",
      "BTT K-Touch",
    );
    expect(s.heading).toBe("HelixScreen is installed");
    expect(s.lines.join(" ")).not.toMatch(/Nothing on your printer was changed/);
    expect(s.lines[0]).toMatch(/^HelixScreen 1\.1\.0 was written/);
  });
});

describe("the finish screen of an update", () => {
  it("names the version an update installed", () => {
    const s = finishScreen("note", "the BTT K-Touch restarted running HelixScreen v1.1.0-beta.10", "BTT K-Touch", "v1.1.0-beta.10");
    expect(s.heading).toBe("HelixScreen updated to v1.1.0-beta.10");
  });
  it("says installed only for a first install", () => {
    expect(finishScreen(null, null, "Creality K1", null).heading).toBe("HelixScreen is installed");
    expect(finishScreen(null, null, "Creality K1", "v1.0.5").heading).toBe("HelixScreen updated to v1.0.5");
  });
});
