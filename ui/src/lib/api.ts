// SPDX-License-Identifier: GPL-3.0-or-later
// The Tauri commands, typed, with every failure turned into an ApiError whose message a
// first-time user can act on. The engine's own wording stays in `detail` for the support report.
import { invoke } from "@tauri-apps/api/core";
import type {
  AdvanceView,
  AppInfo,
  CandidateView,
  Input,
  PrinterView,
  RunSummary,
} from "./types";

export type ErrorKind =
  | "no_printer_list"
  | "unknown_printer"
  | "unknown_run"
  | "not_an_address"
  | "choose_model"
  | "unsupported"
  | "storage"
  | "run_committed"
  | "unexpected";

const MESSAGES: Record<ErrorKind, string> = {
  no_printer_list:
    "The installer couldn't load its list of printers. Check that this computer is connected to the internet, then try again.",
  unknown_printer: "That printer isn't in the list any more. Search again.",
  unknown_run: "That install can't be found on this computer any more.",
  not_an_address:
    "That doesn't look like a printer address. It should be four numbers with dots, like 192.168.1.50.",
  choose_model: "Pick your printer's model first.",
  unsupported: "The installer can't set up this printer.",
  storage:
    "The installer couldn't save its progress on this computer. Check that the disk isn't full, then try again.",
  run_committed:
    "This install has already changed your printer, so it can't be put aside. Continue it to finish setting up the printer.",
  unexpected: "The installer couldn't do that. Try again, or close the installer and open it again.",
};

export class ApiError extends Error {
  constructor(
    public kind: ErrorKind,
    message: string,
    /** The engine's own words, for the support report. */
    public detail: string,
    /** A guide to follow instead, when there is one. */
    public doc: string | null,
  ) {
    super(message);
  }
}

function isKind(k: unknown): k is ErrorKind {
  return typeof k === "string" && Object.hasOwn(MESSAGES, k);
}

export function toApiError(e: unknown): ApiError {
  if (e && typeof e === "object" && "kind" in e) {
    const o = e as { kind: unknown; message?: unknown; doc?: unknown };
    const detail = typeof o.message === "string" ? o.message : "";
    const doc = typeof o.doc === "string" ? o.doc : null;
    if (isKind(o.kind)) return new ApiError(o.kind, MESSAGES[o.kind], detail, doc);
    return new ApiError("unexpected", MESSAGES.unexpected, detail, doc);
  }
  const detail = typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
  return new ApiError("unexpected", MESSAGES.unexpected, detail, null);
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw toApiError(e);
  }
}

export const appInfo = () => call<AppInfo>("app_info");
export const printers = () => call<PrinterView[]>("printers");
export const discover = () => call<CandidateView[]>("discover");
export const addManualHost = (ip: string) => call<CandidateView>("add_manual_host", { ip });
export const startRun = (candidateKey: string, printerId?: string | null) =>
  call<string>("start_run", { candidateKey, printerId: printerId ?? null });
export const advance = (runId: string, input?: Input | null) =>
  call<AdvanceView>("advance", { runId, input: input ?? null });
export const unfinishedRuns = () => call<RunSummary[]>("unfinished_runs");
export const dismissRun = (runId: string) => call<void>("dismiss_run", { runId });
export const exportBundle = (runId: string, mask: boolean) =>
  call<string>("export_bundle", { runId, mask });
export const recordPrinter = (candidateKey: string) =>
  call<string>("record_printer", { candidateKey });

/** What to tell the user when starting the printer they picked failed. A refusal of the model
 * they chose says to choose again rather than only that the printer can't be set up. */
export function pickFailure(e: ApiError): string {
  if (e.kind !== "unsupported") return e.message;
  if (e.detail.includes("answers as a different model")) {
    return "This printer answers as a different model from the one you picked. Pick another model, or go back to the printers found.";
  }
  if (e.detail === "printer not recognised") {
    return "Your printer answered, but it isn't one the installer knows. The install guide shows how to set it up by hand.";
  }
  return e.message;
}
