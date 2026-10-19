// SPDX-License-Identifier: GPL-3.0-or-later
import { describe, expect, it } from "vitest";
import { signInInput, sudoInput } from "./login";

describe("signInInput", () => {
  it("sends the user and password for a password sign-in", () => {
    expect(signInInput({ user: " pi ", password: "raspberry", useKey: false, passphrase: "left over" })).toEqual({
      input: "login",
      value: { user: "pi", password: "raspberry" },
    });
  });
  it("sends no password for a key sign-in, and the passphrase only when one was typed", () => {
    expect(signInInput({ user: "pi", password: "typed before", useKey: true, passphrase: "" })).toEqual({
      input: "login",
      value: { user: "pi", password: "" },
    });
    expect(signInInput({ user: "pi", password: "", useKey: true, passphrase: "open sesame" })).toEqual({
      input: "login",
      value: { user: "pi", password: "", passphrase: "open sesame" },
    });
  });
});

describe("sudoInput", () => {
  it("answers the sudo password prompt", () => {
    expect(sudoInput("s3cret")).toEqual({ input: "sudo_password", value: "s3cret" });
  });
});
