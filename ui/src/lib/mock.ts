// SPDX-License-Identifier: GPL-3.0-or-later
// Development only: answers the Tauri commands in a plain browser so each screen can be seen
// and captured without the Rust side. Loaded by main.ts in `vite --mode mock` and nowhere else.
// `?screen=<name>` picks what the install shows after the first printer's button is pressed.
import { mockIPC } from "@tauri-apps/api/mocks";
import type {
  AdvanceView,
  CandidateView,
  PrinterView,
  RunEvent,
  RunInfo,
  RunSummary,
} from "./types";

/** A drawn stand-in for a manifest picture, labelled so a screenshot shows where it sits. */
function picture(label: string, w = 480, h = 270, hue = 210): string {
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}">
<rect width="${w}" height="${h}" fill="hsl(${hue} 30% 22%)"/>
<rect x="${w * 0.08}" y="${h * 0.12}" width="${w * 0.84}" height="${h * 0.76}" rx="10" fill="hsl(${hue} 25% 32%)"/>
<text x="50%" y="52%" fill="#e8edf2" font-family="sans-serif" font-size="${Math.round(h / 11)}" text-anchor="middle">${label}</text>
</svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

const printers: PrinterView[] = [
  {
    id: "k1",
    name: "Creality K1",
    photo: picture("K1", 200, 200, 200),
    verified: { by: "helixscreen", date: "2026-10-01", firmware: "1.3.3.46" },
  },
  { id: "k2", name: "Creality K2", photo: picture("K2", 200, 200, 20), verified: null },
  {
    id: "ad5m",
    name: "FlashForge Adventurer 5M",
    photo: picture("AD5M", 200, 200, 150),
    verified: { by: "community", date: "", firmware: "" },
  },
  { id: "ad5x", name: "FlashForge Adventurer 5X", photo: "img/missing-ad5x.jpg", verified: null },
  {
    id: "q2",
    name: "QIDI Q2",
    photo: picture("Q2", 200, 200, 280),
    verified: { by: "helixscreen", date: "", firmware: "1.0.4" },
  },
  { id: "pi", name: "Raspberry Pi with Klipper", photo: null, verified: null },
];

const found: CandidateView[] = [
  {
    key: "SN-K1",
    address: "192.168.1.50",
    name: "Creality K1",
    photo: printers[0].photo,
    printer_id: "k1",
    status: "ready",
    answered: true,
    helix_version: null,
    choices: [],
    doc: null,
    reason: null,
  },
  {
    key: "SN-Q2",
    address: "192.168.1.61",
    name: "QIDI Q2",
    photo: printers[4].photo,
    printer_id: "q2",
    status: "ready",
    answered: true,
    helix_version: "1.0.3",
    choices: [],
    doc: null,
    reason: null,
  },
  {
    key: "192.168.1.77",
    address: "192.168.1.77",
    name: "Ender-3 V3",
    photo: null,
    printer_id: null,
    status: "unsupported",
    answered: true,
    helix_version: null,
    choices: [],
    doc: "https://helixscreen.org/docs/install",
    reason: "printer not recognised",
  },
];

const run = (over: Partial<RunInfo> = {}): RunInfo => ({
  run_id: "run-mock",
  printer_name: "Creality K1",
  address: "192.168.1.50",
  step: 2,
  total_steps: 9,
  step_kind: "info",
  committed: false,
  guide: "https://helixscreen.org/docs/install-k1",
  done_note: null,
  ...over,
});

const MB = 1000 * 1000;

const screens: Record<string, { event: RunEvent; run: RunInfo }> = {
  info: {
    event: {
      event: "need_input",
      prompt: "info",
      text: "Restoring your K1 to its original software later needs a factory reset.\n\nThe installer will now root your printer and install HelixScreen. This takes about 15 minutes.",
      image: picture("K1 settings screen"),
    },
    run: run({ step: 1 }),
  },
  user_action: {
    event: {
      event: "need_input",
      prompt: "user_action",
      text: "On the printer's screen, open Settings, then General, and turn on Root account. Wait for the printer to show the root password, then come back here.",
      image: picture("Settings > General > Root account"),
    },
    run: run({ step: 2, step_kind: "user_action" }),
  },
  ask: {
    event: {
      event: "need_input",
      prompt: "ask",
      question: "What version does the About screen show?",
      choices: [
        { value: "1.1", label: "1.1.x", image: picture("About: 1.1.2", 320, 180, 200) },
        { value: "1.2", label: "1.2.x", image: picture("About: 1.2.3", 320, 180, 160) },
        { value: "3.0", label: "3.0 or newer", image: picture("About: 3.0.9", 320, 180, 30) },
        { value: "unsure", label: "I can't find it", image: null },
      ],
    },
    run: run({ printer_name: "FlashForge Adventurer 5X", step: 1, total_steps: 8, step_kind: "ask" }),
  },
  login: {
    event: {
      event: "need_input",
      prompt: "login",
      user_hint: "root",
      password_hint: null,
      has_ssh_key: true,
      key_locked: true,
    },
    run: run({ step: 3, step_kind: "ssh_login" }),
  },
  sudo: {
    event: { event: "need_input", prompt: "sudo_password", user: "pi", refused: false },
    run: run({ step: 4, step_kind: "helix_install" }),
  },
  drive: {
    event: {
      event: "need_input",
      prompt: "choose_drive",
      needed_bytes: 312 * MB,
      message: null,
      drives: [
        {
          id: "sdb1",
          label: "SanDisk Ultra",
          mount: "/media/me/SANDISK",
          fs: "vfat",
          size_bytes: 16_000 * MB,
          free_bytes: 15_200 * MB,
          removable: true,
          system: false,
        },
        {
          id: "sdc1",
          label: "USB DISK",
          mount: "/media/me/USB",
          fs: "vfat",
          size_bytes: 8_000 * MB,
          free_bytes: 2_100 * MB,
          removable: true,
          system: false,
        },
      ],
    },
    run: run({ printer_name: "FlashForge Adventurer 5M", step: 4, total_steps: 10, step_kind: "write_stick" }),
  },
  drive_empty: {
    event: {
      event: "need_input",
      prompt: "choose_drive",
      needed_bytes: 312 * MB,
      drives: [],
      message: "No USB stick found. Plug one in (FAT32). Built-in SD card readers are not offered.",
    },
    run: run({ printer_name: "FlashForge Adventurer 5M", step: 4, total_steps: 10, step_kind: "write_stick" }),
  },
  progress: {
    event: {
      event: "progress",
      step: 7,
      total: 9,
      title: "Installing HelixScreen",
      detail: "Step 3 of 6: Copying files to the printer",
      fraction: 0.42,
    },
    run: run({ step: 7, step_kind: "helix_install", committed: true }),
  },
  done: { event: { event: "done" }, run: run({ step: 9, step_kind: null, committed: true }) },
  up_to_date: {
    event: { event: "done" },
    run: run({
      step: 9,
      step_kind: null,
      done_note:
        "HelixScreen is up to date: 1.1.0-beta.9 is installed, and the newest beta release is 1.1.0-beta.9",
    }),
  },
  refused: {
    event: {
      event: "refused",
      doc: "https://helixscreen.org/docs/install-k1",
      reason: "K1 firmware 1.2.9.15 is older than the 1.3.3.5 this install needs",
      cancelled: false,
    },
    run: run({ step: 4, step_kind: "require" }),
  },
  failed: {
    event: {
      event: "failed",
      committed: false,
      step: 5,
      reason: "could not download Simple AF: timed out",
      help: null,
    },
    run: run({ step: 5, step_kind: "download" }),
  },
  recovery: {
    event: {
      event: "failed",
      committed: true,
      step: 7,
      reason: "install.sh step 4 of 6 failed: could not start helixscreen.service",
      help: "https://helixscreen.org/docs/recovery-k1",
    },
    run: run({ step: 7, step_kind: "helix_install", committed: true }),
  },
};

let unfinished: RunSummary[] = [
  {
    run_id: "run-mock",
    printer_name: "Creality K1",
    address: "192.168.1.50",
    step: 5,
    total_steps: 9,
    committed: true,
    reason: "the printer stopped answering",
  },
  {
    run_id: "run-mock-2",
    printer_name: "FlashForge Adventurer 5M",
    address: "192.168.1.62",
    step: 3,
    total_steps: 10,
    committed: false,
    reason: "could not download the firmware: timed out",
  },
];

const BUNDLE = `HelixScreen installer support bundle
run: run-mock
printer: k1
recipe: k1_stock
host: <ip>
status: Failed { reason: "install.sh step 4 of 6 failed", help: None }
step: 7 committed: true
facts:
  model: "K1"
  serial: <serial>
log:
  [1760800000] step 0: info: done
  [1760800042] step 6: passed the point of no return
  [1760800400] step 7: failed: install.sh step 4 of 6 failed
`;

export function install(): void {
  const screen = new URLSearchParams(location.search).get("screen") ?? "picker";
  let advanced = false;
  mockIPC((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "pending_update":
        return null;
      case "app_info":
        return { version: "0.1.0", image_base: null };
      case "printers":
        if (screen === "no_list") {
          throw { kind: "no_printer_list", message: "manifest signature is not valid", doc: null };
        }
        return printers;
      case "discover":
        return new Promise((resolve) => setTimeout(() => resolve(found), 300));
      case "unfinished_runs":
        return screen === "resume" ? unfinished : [];
      case "dismiss_run": {
        const r = unfinished.find((u) => u.run_id === a.runId);
        if (r?.committed) {
          throw { kind: "run_committed", message: "this install has already changed the printer", doc: null };
        }
        unfinished = unfinished.filter((u) => u.run_id !== a.runId);
        return null;
      }
      case "add_manual_host":
        return { ...found[0], key: String(a.ip), address: String(a.ip) };
      case "start_run":
        return "run-mock";
      case "advance": {
        // Continuing a saved run from the resume banner opens it where it failed.
        const s = screens[screen] ?? (screen === "resume" ? screens.recovery : screens.info);
        // A long step: the first answer is progress, the next never comes.
        if (s.event.event === "progress" && advanced) return new Promise(() => {});
        advanced = true;
        return { event: s.event, run: s.run } satisfies AdvanceView;
      }
      case "export_bundle":
        return BUNDLE;
      case "record_printer":
        return JSON.stringify({ model: "K1", serial: "<serial>", has_moonraker: true });
      case "plugin:opener|open_url":
        return null;
      default:
        throw { kind: "unexpected", message: `mock has no ${cmd}`, doc: null };
    }
  });
}
