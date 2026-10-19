// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { readyLine, startLabel, whereLine } from "./candidates";
import type { CandidateView } from "./types";

const candidate = (over: Partial<CandidateView>): CandidateView => ({
  key: "k",
  address: "192.168.1.50",
  connection: "network",
  name: "Printer",
  photo: null,
  printer_id: "p",
  status: "ready",
  answered: true,
  helix_version: null,
  choices: [],
  doc: null,
  reason: null,
  ...over,
});

const ktouch = candidate({ key: "/dev/ttyUSB0", address: "/dev/ttyUSB0", connection: "usb", name: "BTT K-Touch" });

describe("whereLine", () => {
  it("gives a network printer's address", () => {
    expect(whereLine(candidate({}))).toBe("At 192.168.1.50");
  });
  it("says a K-Touch is connected by USB, with its port", () => {
    expect(whereLine(ktouch)).toBe("Connected by USB (/dev/ttyUSB0)");
  });
});

describe("readyLine", () => {
  it("names the installed HelixScreen when the printer said", () => {
    expect(readyLine(candidate({ helix_version: "1.0.3" }))).toBe("HelixScreen 1.0.3 is installed");
    expect(readyLine(candidate({ helix_version: "" }))).toBe("HelixScreen is installed");
  });
  it("is ready to install a network printer without HelixScreen", () => {
    expect(readyLine(candidate({}))).toBe("Ready to install");
  });
  it("does not claim to know what a USB screen runs before its install reads it", () => {
    expect(readyLine(ktouch)).toMatch(/checks what it runs/);
    expect(readyLine(ktouch)).not.toMatch(/Ready to install/);
  });
});

describe("startLabel", () => {
  it("installs or updates a network printer by what it reported", () => {
    expect(startLabel(candidate({}))).toBe("Install HelixScreen");
    expect(startLabel(candidate({ helix_version: "1.0.3" }))).toBe("Update HelixScreen");
  });
  it("offers both for a USB screen, whose version is read once the install starts", () => {
    expect(startLabel(ktouch)).toBe("Install or update HelixScreen");
  });
});
