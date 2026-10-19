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
}

export type CandidateStatus = "ready" | "choose" | "unsupported" | "no_answer";

export interface CandidateView {
  key: string;
  address: string;
  name: string;
  photo: string | null;
  printer_id: string | null;
  status: CandidateStatus;
  answered: boolean;
  helix_version: string | null;
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

export interface RunInfo {
  run_id: string;
  printer_name: string;
  address: string;
  step: number;
  total_steps: number;
  step_kind: string | null;
  committed: boolean;
  guide: string;
  /** Why a run that ended early is done ("HelixScreen is up to date: ..."). */
  done_note: string | null;
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
