// SPDX-License-Identifier: GPL-3.0-or-later
// The sign-in form's answers as the engine reads them. A key sign-in sends no password: the
// engine then signs in with this computer's SSH key alone.
import type { Input } from "./types";

export interface SignInForm {
  user: string;
  password: string;
  useKey: boolean;
  passphrase: string;
}

export function signInInput(f: SignInForm): Input {
  const user = f.user.trim();
  if (!f.useKey) return { input: "login", value: { user, password: f.password } };
  return f.passphrase
    ? { input: "login", value: { user, password: "", passphrase: f.passphrase } }
    : { input: "login", value: { user, password: "" } };
}

export function sudoInput(password: string): Input {
  return { input: "sudo_password", value: password };
}
