// SPDX-License-Identifier: GPL-3.0-or-later
// Development only: answers the Tauri commands in a plain browser so each screen can be seen
// and captured without the Rust side. Loaded by main.ts in `vite --mode mock` and nowhere else.
// `?screen=<name>` picks what the install shows after the first printer's button is pressed.
// `?list=staging` lists the printers of `manifest/staging.json` instead of the stand-ins, and
// manifest images (served from `manifest/` in this mode) load as the app loads them.
import { setHandler } from "./mock-core";
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
    usb_only: false,
  },
  { id: "k2", name: "Creality K2", photo: picture("K2", 200, 200, 20), verified: null, usb_only: false },
  {
    id: "ad5m",
    name: "FlashForge Adventurer 5M",
    photo: picture("AD5M", 200, 200, 150),
    verified: { by: "community", date: "", firmware: "" },
    usb_only: false,
  },
  { id: "ad5x", name: "FlashForge Adventurer 5X", photo: "img/missing-ad5x.jpg", verified: null, usb_only: false },
  {
    id: "q2",
    name: "QIDI Q2",
    photo: picture("Q2", 200, 200, 280),
    verified: { by: "helixscreen", date: "", firmware: "1.0.4" },
    usb_only: false,
  },
  { id: "pi", name: "Raspberry Pi with Klipper", photo: null, verified: null, usb_only: false },
];

const found: CandidateView[] = [
  {
    key: "SN-K1",
    address: "192.168.1.50",
    connection: "network",
    name: "Creality K1",
    photo: printers[0].photo,
    printer_id: "k1",
    status: "ready",
    answered: true,
    helix_version: null,
    host_name: "k1-printer",
    choices: [],
    doc: null,
    reason: null,
  },
  {
    key: "SN-Q2",
    address: "192.168.1.61",
    connection: "network",
    name: "QIDI Q2",
    photo: printers[4].photo,
    printer_id: "q2",
    status: "ready",
    answered: true,
    helix_version: "1.0.3",
    host_name: "linaro-alip",
    choices: [],
    doc: null,
    reason: null,
  },
  {
    key: "192.168.1.77",
    address: "192.168.1.77",
    connection: "network",
    name: "Ender-3 V3",
    photo: null,
    printer_id: null,
    status: "unsupported",
    answered: true,
    helix_version: null,
    host_name: null,
    choices: [],
    doc: "https://helixscreen.org/installation/",
    reason: "printer not recognised",
  },
  {
    key: "/dev/ttyUSB0",
    address: "/dev/ttyUSB0",
    connection: "usb",
    name: "BTT K-Touch",
    photo: null,
    printer_id: "ktouch",
    status: "ready",
    answered: true,
    helix_version: null,
    host_name: null,
    choices: [],
    doc: null,
    reason: null,
  },
];

const forgex: CandidateView = {
  key: "192.168.1.70",
  address: "192.168.1.70",
  connection: "network",
  name: "FlashForge Adventurer 5X with Forge-X",
  photo: null,
  printer_id: "ad5x_forgex",
  status: "unsupported",
  answered: true,
  helix_version: null,
  host_name: "ad5x",
  choices: [],
  doc: "https://helixscreen.org/guide/install-ad5x/",
  reason: "this installer cannot put HelixScreen on a FlashForge Adventurer 5X with Forge-X",
};

const klipperHost = {
  printer_name: "Raspberry Pi with Klipper",
  address: "192.168.1.163",
  host_name: "helixpi",
  guide: "https://helixscreen.org/installation/",
};

const ktouchRun = {
  printer_name: "BTT K-Touch",
  address: "/dev/ttyUSB0",
  host_name: null,
  guide: "https://helixscreen.org/1.1/guide/install-esp32/",
};

const run = (over: Partial<RunInfo> = {}): RunInfo => ({
  run_id: "run-mock",
  printer_name: "Creality K1",
  address: "192.168.1.50",
  step: 2,
  total_steps: 9,
  step_kind: "info",
  committed: false,
  guide: "https://helixscreen.org/guide/install-k1/",
  done_note: null,
  done_kind: null,
  host_name: "k1-printer",
  updated_to: null,
  finish_notes: [],
  set_aside: null,
  ...over,
});

const MB = 1000 * 1000;

const screens: Record<string, { event: RunEvent; run: RunInfo }> = {
  confirm: {
    event: {
      event: "need_input",
      prompt: "confirm",
      text: "The BTT K-Touch is plugged in at /dev/ttyUSB0. The installer will now connect to it, which restarts it. If /dev/ttyUSB0 is some other device, unplug it or stop here.",
      button: "Continue",
    },
    run: run({ ...ktouchRun, step: 0, total_steps: 6, step_kind: "probe" }),
  },
  warn: {
    event: {
      event: "need_input",
      prompt: "confirm",
      text: "Firmware 3.1.5 can be missing the printer's configuration files: FlashForge's own 3.1.5 image does not include them, so a printer that was reset or wiped and then given that image has none, and Forge-X will not work on it. If this printer prints normally now, it has them and you can continue. If you are not sure, stop and flash FlashForge's Factory image first, then run the installer again.",
      button: "Continue anyway",
      doc: "https://github.com/DrA1ex/ff5m/blob/main/docs/UNINSTALL.md#flashing-factory-firmware",
      warn: true,
    },
    run: run({ printer_name: "FlashForge Adventurer 5M", address: "192.168.1.62", host_name: "ad5m", step: 3, total_steps: 17, step_kind: "require" }),
  },
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
  login_key: {
    event: {
      event: "need_input",
      prompt: "login",
      user_hint: "pi",
      password_hint: null,
      has_ssh_key: true,
      key_locked: false,
    },
    run: run({ ...klipperHost, step: 0, total_steps: 4, step_kind: "ssh_login" }),
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
    run: run({ ...klipperHost, step: 2, total_steps: 4, step_kind: "helix_install" }),
  },
  k1_user_action: {
    event: {
      event: "need_input",
      prompt: "user_action",
      text: "Turn on root access: on the printer's screen open Settings, then Root account information. Read the notice, wait for the countdown, then press OK. Then press I've done this.",
      image: "k1/root-account.png",
    },
    run: run({ printer_name: "Creality K1 / K1C / K1 Max", step: 4, total_steps: 21, step_kind: "user_action" }),
  },
  ktouch_blank: {
    event: {
      event: "failed",
      committed: true,
      step: 4,
      reason:
        "the BTT K-Touch disconnected while it was being written; the BTT K-Touch was erased for the install and is blank until a flash completes",
      help: "The BTT K-Touch shows nothing and will not start until a flash completes. Keep it plugged in (plug it back in if it came loose) and choose Try this step again to write it.",
    },
    run: run({ ...ktouchRun, step: 4, total_steps: 6, step_kind: "serial_flash", committed: true }),
  },
  ktouch_done: {
    event: { event: "done" },
    run: run({
      ...ktouchRun,
      step: 4,
      total_steps: 4,
      step_kind: null,
      committed: true,
      done_note: "the BTT K-Touch restarted running HelixScreen v1.1.0-beta.10, and its WiFi and settings were kept",
      done_kind: "note",
      updated_to: "v1.1.0-beta.10",
    }),
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
  done_kept: {
    event: { event: "done" },
    run: run({
      step: 9,
      step_kind: null,
      committed: true,
      finish_notes: [
        {
          step: 21,
          text: 'your 3 print files from before the install were kept: they are in the folder "From Creality firmware"',
          warn: false,
        },
      ],
    }),
  },
  done_kept_short: {
    event: { event: "done" },
    run: run({
      step: 9,
      step_kind: null,
      committed: true,
      finish_notes: [
        {
          step: 21,
          text: 'the installer set aside 3 print files before the install, and only 2 reached the folder "From Creality firmware". The rest are still on the printer in /usr/data/printer_data/gcodes/.helix-kept',
          warn: true,
        },
      ],
    }),
  },
  up_to_date: {
    event: { event: "done" },
    run: run({
      step: 9,
      step_kind: null,
      done_note: "1.1.0-beta.9 is installed, and the newest beta release is 1.1.0-beta.9",
      done_kind: "up_to_date",
    }),
  },
  refused_forgex: {
    event: {
      event: "refused",
      doc: "https://helixscreen.org/guide/install-ad5x/",
      reason: "this installer cannot put HelixScreen on a FlashForge Adventurer 5X with Forge-X",
      cancelled: false,
    },
    run: run({
      printer_name: "FlashForge Adventurer 5X",
      address: "192.168.1.70",
      host_name: "ad5x",
      step: 1,
      total_steps: 16,
      step_kind: "probe",
      guide: "https://helixscreen.org/guide/install-ad5x/",
    }),
  },
  refused: {
    event: {
      event: "refused",
      doc: "https://helixscreen.org/guide/install-k1/",
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
      help: "https://helixscreen.org/guide/install-k1/",
    },
    run: run({ step: 7, step_kind: "helix_install", committed: true }),
  },
  recovery_kept: {
    event: {
      event: "failed",
      committed: true,
      step: 16,
      reason: "the command failed (exit 1): FATAL: could not update /usr/data/pellcorp (git pull failed)",
      help: "https://helixscreen.org/guide/install-k1/",
    },
    run: run({
      step: 16,
      total_steps: 26,
      step_kind: "ssh_run",
      committed: true,
      set_aside: { count: 3, path: "/usr/data/printer_data/gcodes/.helix-kept" },
    }),
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

/** The printer list in `manifest/staging.json`, as the app's `printers` command gives it. */
async function stagingPrinters(): Promise<PrinterView[]> {
  const m = await (await fetch("/staging.json")).json();
  type Entry = Omit<PrinterView, "usb_only"> & { identify: Record<string, unknown>[] };
  return m.printers.map((p: Entry) => ({
    id: p.id,
    name: p.name,
    photo: p.photo ?? null,
    verified: p.verified ?? null,
    usb_only: p.identify.length > 0 && p.identify.every((r) => "usb" in r),
  }));
}

export function install(): void {
  const params = new URLSearchParams(location.search);
  const screen = params.get("screen") ?? "picker";
  const staging = params.get("list") === "staging";
  let advanced = false;
  setHandler((cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "pending_update":
        return null;
      case "app_info":
        return { version: "0.1.0", image_base: `${location.origin}/img/` };
      case "printers":
        if (screen === "no_list") {
          throw { kind: "no_printer_list", message: "manifest signature is not valid", doc: null };
        }
        return staging ? stagingPrinters() : printers;
      case "discover": {
        const list = screen === "picker_forgex" ? [found[0], forgex, found[3]] : found;
        return new Promise((resolve) => setTimeout(() => resolve(list), 300));
      }
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
