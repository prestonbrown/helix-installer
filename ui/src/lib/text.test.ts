// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { confirmHeading, finishScreen, setAsideText, unchangedLines, keepOnLabel, keepPowerText, retryAdvice, stepLabel, stoppedState } from "./text";

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
  it("adds what the run kept after the install line, and shows a shortfall as a warning", () => {
    const s = finishScreen(null, null, "Creality K1", null, [
      { step: 21, text: 'your 3 print files from before the install were kept: they are in the folder "Kept"', warn: false },
      { step: 22, text: "only 2 came back", warn: true },
    ]);
    expect(s.lines[0]).toMatch(/Your Creality K1 may restart/);
    expect(s.lines[1]).toBe('Your 3 print files from before the install were kept: they are in the folder "Kept".');
    expect(s.warnings).toEqual(["Only 2 came back."]);
  });
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

describe("a confirm", () => {
  it("that warns opens with a caution heading", () => {
    expect(confirmHeading(true)).toBe("Check this before you continue");
  });
  it("that is routine has no heading", () => {
    expect(confirmHeading(false)).toBeNull();
    expect(confirmHeading(undefined)).toBeNull();
  });
});

describe("the recovery screen", () => {
  it("names where set-aside print files are and how they come back", () => {
    const t = setAsideText({ count: 3, path: "/usr/data/printer_data/gcodes/.helix-kept" });
    expect(t).toMatch(/^Your 3 print files are safe on the printer in \/usr\/data\/printer_data\/gcodes\/\.helix-kept\./);
    expect(t).toMatch(/running the installer on this printer again/);
    expect(setAsideText({ count: 1, path: "/g" })).toMatch(/^Your 1 print file is safe/);
  });
});

describe("a run that stopped before its point of no return", () => {
  const putBack = { step: 4, text: 'your 2 print files from an earlier run were put back in the folder "Kept"', warn: false };
  it("says nothing changed when nothing did", () => {
    expect(unchangedLines([])).toEqual(["Nothing on your printer was changed."]);
  });
  it("names print files a restore moved, and only then says nothing else changed", () => {
    expect(unchangedLines([putBack])).toEqual([
      'Your 2 print files from an earlier run were put back in the folder "Kept".',
      "Nothing else on your printer was changed.",
    ]);
  });
  it("does the same on an up-to-date finish", () => {
    const s = finishScreen("up_to_date", "1.1.0 is installed", "Creality K1", null, [putBack]);
    expect(s.lines).toContain("Nothing else on your printer was changed.");
    expect(s.lines.join(" ")).toMatch(/put back/);
  });
});
