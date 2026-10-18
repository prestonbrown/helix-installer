// SPDX-License-Identifier: GPL-3.0-or-later
import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));

import * as api from "./api";
import { ApiError } from "./api";

function rejectWith(kind: string, message = "engine detail", doc: string | null = null) {
  invoke.mockRejectedValueOnce({ kind, message, doc });
}

async function failure(p: Promise<unknown>): Promise<ApiError> {
  try {
    await p;
  } catch (e) {
    expect(e).toBeInstanceOf(ApiError);
    return e as ApiError;
  }
  throw new Error("the call did not fail");
}

beforeEach(() => invoke.mockReset());

describe("appInfo", () => {
  it("passes the answer through", async () => {
    invoke.mockResolvedValueOnce({ version: "0.1.0", image_base: null });
    expect(await api.appInfo()).toEqual({ version: "0.1.0", image_base: null });
    expect(invoke).toHaveBeenCalledWith("app_info", undefined);
  });
  it("turns a bare string rejection into an unexpected error that keeps the text", async () => {
    invoke.mockRejectedValueOnce("command app_info not found");
    const e = await failure(api.appInfo());
    expect(e.kind).toBe("unexpected");
    expect(e.detail).toBe("command app_info not found");
    expect(e.message).not.toContain("command app_info");
  });
});

describe("printers", () => {
  it("says the printer list could not load, in plain words", async () => {
    rejectWith("no_printer_list", "manifest signature is not valid");
    const e = await failure(api.printers());
    expect(e.kind).toBe("no_printer_list");
    expect(e.message).toMatch(/list of printers/);
    expect(e.message).not.toMatch(/manifest/i);
    expect(e.detail).toBe("manifest signature is not valid");
  });
});

describe("discover", () => {
  it("returns the printers found", async () => {
    invoke.mockResolvedValueOnce([{ key: "a" }]);
    expect(await api.discover()).toEqual([{ key: "a" }]);
    expect(invoke).toHaveBeenCalledWith("discover", undefined);
  });
  it("maps a missing printer list", async () => {
    rejectWith("no_printer_list");
    expect((await failure(api.discover())).kind).toBe("no_printer_list");
  });
});

describe("addManualHost", () => {
  it("sends the typed address", async () => {
    invoke.mockResolvedValueOnce({ key: "10.0.0.9" });
    await api.addManualHost("10.0.0.9");
    expect(invoke).toHaveBeenCalledWith("add_manual_host", { ip: "10.0.0.9" });
  });
  it("explains what an address looks like", async () => {
    rejectWith("not_an_address", 'not an IP address: "printer!"');
    const e = await failure(api.addManualHost("printer!"));
    expect(e.kind).toBe("not_an_address");
    expect(e.message).toMatch(/192\.168\.1\.50/);
  });
});

describe("startRun", () => {
  it("sends the key and a null model when none is picked", async () => {
    invoke.mockResolvedValueOnce("run-1");
    expect(await api.startRun("SN42")).toBe("run-1");
    expect(invoke).toHaveBeenCalledWith("start_run", { candidateKey: "SN42", printerId: null });
  });
  it("keeps the guide link of an unsupported printer", async () => {
    rejectWith("unsupported", "no install recipe", "https://helixscreen.org/docs/k1");
    const e = await failure(api.startRun("SN42", "k1"));
    expect(e.kind).toBe("unsupported");
    expect(e.doc).toBe("https://helixscreen.org/docs/k1");
    expect(e.message).toMatch(/can't set up this printer/);
  });
  it("asks for the model", async () => {
    rejectWith("choose_model");
    expect((await failure(api.startRun("x"))).message).toMatch(/model/);
  });
  it("says a vanished printer must be searched for again", async () => {
    rejectWith("unknown_printer");
    expect((await failure(api.startRun("x"))).message).toMatch(/Search again/);
  });
});

describe("pendingUpdate", () => {
  it("reads the version the startup check found, or null", async () => {
    invoke.mockResolvedValueOnce("0.2.0");
    expect(await api.pendingUpdate()).toBe("0.2.0");
    expect(invoke).toHaveBeenCalledWith("pending_update", undefined);
    invoke.mockResolvedValueOnce(null);
    expect(await api.pendingUpdate()).toBeNull();
  });
});

describe("advance", () => {
  it("sends the run id and the answer", async () => {
    invoke.mockResolvedValueOnce({ event: { event: "done" }, run: {} });
    await api.advance("run-1", { input: "choice", value: "home" });
    expect(invoke).toHaveBeenCalledWith("advance", {
      runId: "run-1",
      input: { input: "choice", value: "home" },
    });
  });
  it("sends a null input to continue", async () => {
    invoke.mockResolvedValueOnce({ event: { event: "done" }, run: {} });
    await api.advance("run-1");
    expect(invoke).toHaveBeenCalledWith("advance", { runId: "run-1", input: null });
  });
  it("never carries a typed password into the error", async () => {
    rejectWith("unknown_run", 'no run "run-1"');
    const e = await failure(
      api.advance("run-1", { input: "login", value: { user: "root", password: "pw-123" } }),
    );
    expect(e.kind).toBe("unknown_run");
    expect(JSON.stringify(e) + e.message + e.detail).not.toContain("pw-123");
  });
  it("reports storage trouble plainly", async () => {
    rejectWith("storage", "Permission denied (os error 13)");
    const e = await failure(api.advance("run-1"));
    expect(e.kind).toBe("storage");
    expect(e.message).toMatch(/save/);
  });
});

describe("unfinishedRuns", () => {
  it("returns the saved runs", async () => {
    invoke.mockResolvedValueOnce([]);
    expect(await api.unfinishedRuns()).toEqual([]);
    expect(invoke).toHaveBeenCalledWith("unfinished_runs", undefined);
  });
  it("maps storage errors", async () => {
    rejectWith("storage");
    expect((await failure(api.unfinishedRuns())).kind).toBe("storage");
  });
});

describe("dismissRun", () => {
  it("sends the run id", async () => {
    invoke.mockResolvedValueOnce(null);
    await api.dismissRun("run-1");
    expect(invoke).toHaveBeenCalledWith("dismiss_run", { runId: "run-1" });
  });
  it("says a run that changed the printer can only be continued", async () => {
    rejectWith("run_committed", "this install has already changed the printer");
    const e = await failure(api.dismissRun("run-1"));
    expect(e.kind).toBe("run_committed");
    expect(e.message).toMatch(/already changed your printer/);
    expect(e.message).toMatch(/Continue/);
  });
});

describe("pickFailure", () => {
  const unsupported = (detail: string) =>
    new ApiError("unsupported", "The installer can't set up this printer.", detail, "https://x");
  it("asks for another model when the printer answers as a different one", () => {
    const m = api.pickFailure(unsupported("this printer answers as a different model, not a Creality K1"));
    expect(m).toMatch(/different model/);
    expect(m).toMatch(/[Pp]ick another model/);
  });
  it("says the printer answered but is not one the installer knows", () => {
    expect(api.pickFailure(unsupported("printer not recognised"))).toMatch(/isn't one the installer knows/);
  });
  it("keeps the plain message for any other refusal", () => {
    const e = unsupported("no install recipe for this Creality K1 setup");
    expect(api.pickFailure(e)).toBe(e.message);
    const other = new ApiError("storage", "disk full", "", null);
    expect(api.pickFailure(other)).toBe("disk full");
  });
});

describe("exportBundle", () => {
  it("sends the mask choice", async () => {
    invoke.mockResolvedValueOnce("report");
    expect(await api.exportBundle("run-1", false)).toBe("report");
    expect(invoke).toHaveBeenCalledWith("export_bundle", { runId: "run-1", mask: false });
  });
  it("maps an unknown run", async () => {
    rejectWith("unknown_run");
    expect((await failure(api.exportBundle("x", true))).kind).toBe("unknown_run");
  });
});

describe("recordPrinter", () => {
  it("sends the candidate key", async () => {
    invoke.mockResolvedValueOnce("{}");
    await api.recordPrinter("SN42");
    expect(invoke).toHaveBeenCalledWith("record_printer", { candidateKey: "SN42" });
  });
  it("maps an unknown printer", async () => {
    rejectWith("unknown_printer");
    expect((await failure(api.recordPrinter("x"))).kind).toBe("unknown_printer");
  });
});

describe("error shapes", () => {
  it("treats an unknown kind as unexpected", async () => {
    rejectWith("brand_new_kind", "something new");
    const e = await failure(api.discover());
    expect(e.kind).toBe("unexpected");
    expect(e.detail).toBe("something new");
  });
  it("treats a thrown Error as unexpected", async () => {
    invoke.mockRejectedValueOnce(new Error("ipc closed"));
    const e = await failure(api.discover());
    expect(e.kind).toBe("unexpected");
    expect(e.detail).toBe("ipc closed");
  });
});
