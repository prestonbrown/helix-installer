// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { cardSubtitle, cardTitle, nameLines, readyLine, startLabel, whereLine } from "./candidates";
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
  host_name: null,
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

const klipperHost = (host: string | null) =>
  candidate({ name: "Klipper host (Voron, MainsailOS, FluiddPi, KIAUH)", host_name: host, helix_version: "v1.1.0" });

const model = (id: string, name: string) => ({ id, name, photo: null, verified: null });

describe("the card's title and subtitle", () => {
  it("titles a printer by its own host name, with what the installer takes it for underneath", () => {
    const c = klipperHost("voronv0");
    expect(cardTitle(c)).toBe("voronv0");
    expect(cardSubtitle(c)).toBe("Klipper host (Voron, MainsailOS, FluiddPi, KIAUH)");
  });
  it("tells apart printers that run the same recipe", () => {
    const titles = ["bigtreetech-cb1", "helixpi", "voronv0", "mainsailos"].map((h) => cardTitle(klipperHost(h)));
    expect(new Set(titles).size).toBe(4);
  });
  it("drops a MAC-like suffix the host name plainly carries", () => {
    expect(cardTitle(klipperHost("voron-b827eb1a2b3c"))).toBe("voron");
    expect(cardTitle(klipperHost("k1-dc:a6:32:01:02:03"))).toBe("k1");
    expect(cardTitle(klipperHost("mainsailos-1a2b3c"))).toBe("mainsailos");
  });
  it("keeps any other suffix as the printer has it", () => {
    expect(cardTitle(klipperHost("voron-2024"))).toBe("voron-2024");
    expect(cardTitle(klipperHost("printer-facade"))).toBe("printer-facade");
    expect(cardTitle(klipperHost("bigtreetech-cb1"))).toBe("bigtreetech-cb1");
  });
  it("uses the display name alone when the printer gives no host name", () => {
    const c = klipperHost(null);
    expect(cardTitle(c)).toBe("Klipper host (Voron, MainsailOS, FluiddPi, KIAUH)");
    expect(cardSubtitle(c)).toBeNull();
    expect(cardTitle(klipperHost("  "))).toBe("Klipper host (Voron, MainsailOS, FluiddPi, KIAUH)");
  });
  it("names an ambiguous printer by the models it could be, never by an id or host name", () => {
    const c = candidate({
      status: "choose",
      name: "ad5m",
      host_name: "ad5m",
      printer_id: null,
      choices: [model("ad5m", "FlashForge Adventurer 5M"), model("ad5m_pro", "FlashForge Adventurer 5M Pro")],
    });
    expect(cardTitle(c)).toBe("FlashForge Adventurer 5M or 5M Pro");
    expect(cardSubtitle(c)).toBeNull();
  });
  it("lists three or more possible models", () => {
    const c = candidate({
      status: "choose",
      choices: [model("a", "Creality K1"), model("b", "Creality K1C"), model("c", "Creality K1 Max")],
    });
    expect(cardTitle(c)).toBe("Creality K1, K1C or K1 Max");
  });
  it("joins models with nothing in common in full", () => {
    const c = candidate({ status: "choose", choices: [model("a", "Creality K1"), model("b", "Sovol SV08")] });
    expect(cardTitle(c)).toBe("Creality K1 or Sovol SV08");
  });
});

describe("a run's header", () => {
  it("leads with the name the printer goes by, like its card did", () => {
    expect(nameLines("Klipper host (Voron)", "voronv0")).toEqual({
      title: "voronv0",
      subtitle: "Klipper host (Voron)",
    });
    expect(nameLines("Klipper host (Voron)", "voron-b827eb1a2b3c").title).toBe("voron");
  });
  it("uses the printer's display name alone when it gave no name of its own", () => {
    expect(nameLines("Creality K1", null)).toEqual({ title: "Creality K1", subtitle: null });
  });
});
