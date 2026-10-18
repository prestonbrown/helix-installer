// SPDX-License-Identifier: GPL-3.0-or-later
// Runs the Tauri CLI with the signing credentials this build uses, and only those. The release
// workflow gives each credential to this one step as IN_<NAME>; it is passed on as <NAME> only
// when the SIGN_* switch for its group is on, so an unset credential never reaches the bundler
// as an empty string (which it would try to use). TAURI_CLI names the CLI to run, for tests.
const { spawnSync } = require("node:child_process");
const path = require("node:path");

const GROUPS = {
  SIGN_APPLE: ["APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_SIGNING_IDENTITY"],
  NOTARIZE_APPLE: ["APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"],
  SIGN_AZURE: ["AZURE_TENANT_ID", "AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET"],
  SIGN_UPDATER: ["TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD"],
};
// An updater key may have no password; the bundler still needs the variable set.
const MAY_BE_EMPTY = new Set(["TAURI_SIGNING_PRIVATE_KEY_PASSWORD"]);

const env = { ...process.env };
for (const [flag, names] of Object.entries(GROUPS)) {
  for (const name of names) {
    const value = env[`IN_${name}`] ?? "";
    delete env[`IN_${name}`];
    delete env[name];
    if (env[flag] === "true" && (value !== "" || MAY_BE_EMPTY.has(name))) env[name] = value;
  }
}
const cli =
  env.TAURI_CLI || path.join(__dirname, "..", "ui", "node_modules", "@tauri-apps", "cli", "tauri.js");
const run = spawnSync(process.execPath, [cli, ...process.argv.slice(2)], { env, stdio: "inherit" });
process.exit(run.status ?? 1);
