// SPDX-License-Identifier: GPL-3.0-or-later
// The shapes crates/app/src/commands.rs sends and accepts.

export interface Verified {
  by: string;
  date: string;
  firmware: string;
}

export interface PrinterView {
  id: string;
  name: string;
  photo: string | null;
  verified: Verified | null;
  /** Found only by its USB id: it cannot be reached by typing an address. */
  usb_only: boolean;
}

export type CandidateStatus = "ready" | "choose" | "unsupported" | "no_answer";

/** How the installer reaches a candidate; a USB one's `address` is its serial port. */
export type Connection = "network" | "usb";

export interface CandidateView {
  key: string;
  address: string;
  connection: Connection;
  name: string;
  photo: string | null;
  printer_id: string | null;
  status: CandidateStatus;
  answered: boolean;
  helix_version: string | null;
  /** The name the printer's own system goes by, as it reports it. */
  host_name: string | null;
  choices: PrinterView[];
  doc: string | null;
  reason: string | null;
}

export interface AppInfo {
  version: string;
  image_base: string | null;
}

export interface Choice {
  value: string;
  label: string;
  image?: string | null;
}

export interface Drive {
  id: string;
  label: string;
  mount: string;
  fs: string;
  size_bytes: number;
  free_bytes: number;
  removable: boolean;
  system: boolean;
}

export type Prompt =
  | { prompt: "info"; text: string; image: string | null }
  | { prompt: "user_action"; text: string; image: string | null }
  /** The installer is about to act (open a USB port); `button` lets it go ahead. */
  | { prompt: "confirm"; text: string; button: string; doc?: string; warn?: boolean }
  | { prompt: "ask"; question: string; choices: Choice[] }
  | {
      prompt: "login";
      user_hint: string | null;
      password_hint: string | null;
      /** This computer has an SSH key or agent the installer can sign in with. */
      has_ssh_key: boolean;
      /** A key file needs its passphrase before it can be used. */
      key_locked: boolean;
    }
  | { prompt: "sudo_password"; user: string; refused: boolean }
  | { prompt: "choose_drive"; drives: Drive[]; needed_bytes: number; message: string | null }
  | { prompt: "ambiguous"; candidates: string[] };

export type RunEvent =
  | ({ event: "need_input" } & Prompt)
  | {
      event: "progress";
      step: number;
      total: number;
      title: string;
      detail: string | null;
      fraction: number | null;
    }
  | { event: "done" }
  | { event: "refused"; doc: string; reason: string; cancelled: boolean }
  | { event: "failed"; committed: boolean; step: number; reason: string; help: string | null };

/** Why a run ended before its last step. */
export type DoneKind = "up_to_date" | "note";

export interface RunInfo {
  run_id: string;
  printer_name: string;
  address: string;
  step: number;
  total_steps: number;
  step_kind: string | null;
  committed: boolean;
  guide: string;
  /** Why a run that ended early is done ("1.1.0 is installed, and the newest ..."). */
  done_note: string | null;
  done_kind: DoneKind | null;
  /** The name the printer's own system goes by, as on its picker card. */
  host_name: string | null;
  /** The version a finished update installed; null for a first install. */
  updated_to: string | null;
  /** Lines the finish screen adds about what the run kept for the user. */
  finish_notes: FinishNote[];
}

export interface FinishNote {
  step: number;
  text: string;
  /** Something did not go as planned and the user should look. */
  warn: boolean;
}

export interface AdvanceView {
  event: RunEvent;
  run: RunInfo;
}

export interface RunSummary {
  run_id: string;
  printer_name: string;
  address: string;
  step: number;
  total_steps: number | null;
  committed: boolean;
  reason: string | null;
}

export type Input =
  | { input: "next" }
  | { input: "choice"; value: string }
  | { input: "login"; value: { user: string; password: string; passphrase?: string } }
  | { input: "sudo_password"; value: string }
  | { input: "drive"; value: string }
  | { input: "cancel" };
