// SPDX-License-Identifier: GPL-3.0-or-later
// Drives the built app through one update run on a real printer, over WebDriver (tauri-driver on
// E2E_DRIVER, default http://127.0.0.1:4444). It stops at the first screen it does not know how to
// answer safely, so it never confirms anything a recipe asks the user to agree to.
//
//   E2E_APP      the app binary (target/debug/helix_installer_app)
//   E2E_HOST     the printer's address, found by discovery or typed in by hand
//   E2E_MODEL    the start of the model's name to pick when typing the address in (default
//                "Klipper host")
//   E2E_USER     the SSH user name
//   E2E_PW_VAR   the name of the environment variable holding the password; its value is typed
//                into the password fields and never printed
//   E2E_SHOTS    the directory screenshots go to, as NN-<screen>.png
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const DRIVER = process.env.E2E_DRIVER ?? "http://127.0.0.1:4444";
const APP = need("E2E_APP");
const HOST = need("E2E_HOST");
const MODEL = process.env.E2E_MODEL ?? "Klipper host";
const USER = need("E2E_USER");
const PASSWORD = process.env[need("E2E_PW_VAR")];
const SHOTS = need("E2E_SHOTS");
const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";

if (!PASSWORD) fail(`${process.env.E2E_PW_VAR} is empty`);
mkdirSync(SHOTS, { recursive: true });

function need(name) {
  const v = process.env[name];
  if (!v) fail(`${name} is not set`);
  return v;
}

function fail(msg) {
  console.error(`${stamp()} FAIL ${msg}`);
  process.exit(1);
}

function stamp() {
  return new Date().toISOString();
}

function log(msg) {
  console.log(`${stamp()} ${msg}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function wd(method, path, body) {
  const res = await fetch(`${DRIVER}${path}`, {
    method,
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const json = await res.json();
  if (!res.ok) throw new Error(`${method} ${path}: ${JSON.stringify(json.value)}`);
  return json.value;
}

let session = null;
const s = (path) => `/session/${session}${path}`;

async function exec(script, ...args) {
  return wd("POST", s("/execute/sync"), { script, args });
}

async function find(xpath) {
  const found = await wd("POST", s("/elements"), { using: "xpath", value: xpath });
  return found.map((e) => e[ELEMENT]);
}

async function click(id) {
  await wd("POST", s(`/element/${id}/click`), {});
}

async function type(id, text) {
  await wd("POST", s(`/element/${id}/clear`), {});
  await wd("POST", s(`/element/${id}/value`), { text });
}

let shotNo = 0;
async function shot(name) {
  shotNo += 1;
  const file = join(SHOTS, `${String(shotNo).padStart(2, "0")}-${name}.png`);
  writeFileSync(file, Buffer.from(await wd("GET", s("/screenshot")), "base64"));
  log(`screenshot ${file}`);
}

/** The main area's text, one line per block, for telling screens apart and for the log. */
async function mainText() {
  return exec("const m = document.querySelector('main'); return m ? m.innerText : '';");
}

/** The run screen's text and its heading, or null while the picker is still showing. */
async function runScreen() {
  return exec(
    "const r = document.querySelector('main .run'); if (!r) return null;" +
      "const h = r.querySelector('h1'); return { text: r.innerText, heading: h ? h.innerText.trim() : '' };",
  );
}

async function waitFor(what, test, timeoutMs) {
  const end = Date.now() + timeoutMs;
  for (;;) {
    const text = await mainText();
    if (test(text)) return text;
    if (Date.now() > end) fail(`timed out waiting for ${what}; the screen shows:\n${text}`);
    await sleep(500);
  }
}

function slug(text) {
  const first = text.split("\n").find((l) => l.trim()) ?? "blank";
  return first.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "").slice(0, 40) || "blank";
}

async function pickPrinter() {
  const text = await waitFor("the printer search to finish", (t) => !t.includes("Looking for printers"), 90_000);
  if (text.includes(HOST)) {
    await shot("picker-found");
    const cards = await find(`//li[contains(@class,'candidate')][.//*[contains(text(),'${HOST}')]]`);
    if (cards.length !== 1) fail(`expected one card for ${HOST}, found ${cards.length}`);
    const buttons = await find(
      `//li[contains(@class,'candidate')][.//*[contains(text(),'${HOST}')]]//div[contains(@class,'go')]//button`,
    );
    if (buttons.length !== 1) fail(`the card for ${HOST} has no start button:\n${text}`);
    log(`discovery found ${HOST}; starting from its card`);
    await click(buttons[0]);
    return;
  }
  await shot("picker-not-found");
  log(`discovery did not list ${HOST}; typing it in`);
  await click((await find("//button[contains(.,\"My printer isn't listed\")]"))[0]);
  await waitFor("the manual screen", (t) => t.includes("Find your printer"), 5_000);
  const model = await find(`//button[@role='radio'][.//span[starts-with(normalize-space(.),'${MODEL}')]]`);
  if (model.length === 0) fail(`no model button for ${MODEL}`);
  await click(model[0]);
  await type((await find("//input[@placeholder='192.168.1.50']"))[0], HOST);
  await shot("manual");
  await click((await find("//main//button[contains(@class,'primary')]"))[0]);
}

async function signIn() {
  await type((await find("//label[span='User name']/input"))[0], USER);
  await shot("login");
  await type((await find("//label[span='Password']/input"))[0], PASSWORD);
  log(`signing in as ${USER}`);
  await click((await find("//button[@type='submit']"))[0]);
}

async function sudo() {
  await shot("sudo");
  await type((await find("//form//input[@type='password' or @type='text']"))[0], PASSWORD);
  log("giving the sudo password");
  await click((await find("//button[@type='submit']"))[0]);
}

const FINAL = [
  "Nothing to update",
  "HelixScreen is installed",
  "Install stopped",
  "This printer can't be set up automatically",
  "The install stopped",
];

async function main() {
  const created = await wd("POST", "/session", {
    capabilities: { alwaysMatch: { "tauri:options": { application: APP } } },
  });
  session = created.sessionId;
  log(`session ${session}`);
  let outcome = "unknown";
  try {
    await waitFor("the printer list", (t) => !t.includes("Loading the list of printers"), 30_000);
    const first = await mainText();
    if (first.includes("Can't load the printer list")) {
      await shot("no-list");
      fail(`the printer list did not load:\n${first}`);
    }
    await shot("picker-searching");
    await pickPrinter();
    for (let i = 0; !(await runScreen()); i++) {
      if (i > 120) fail(`the run screen never opened; the screen shows:\n${await mainText()}`);
      await sleep(250);
    }

    let last = "";
    const end = Date.now() + 15 * 60_000;
    while (Date.now() < end) {
      const screen = await runScreen();
      if (!screen) fail(`the run screen closed; the screen shows:\n${await mainText()}`);
      const { text, heading } = screen;
      if (text !== last && text.trim()) {
        last = text;
        log(`screen:\n${text.replace(/^/gm, "  | ")}`);
        const final = FINAL.find((f) => heading === f);
        if (final) {
          await shot(slug(final));
          outcome = final;
          break;
        }
        if (heading === "Sign in to your printer") {
          if (text.includes("didn't accept")) {
            await shot("login-refused");
            fail("the printer refused the password");
          }
          await signIn();
        } else if (heading === "Your printer needs your password") {
          if (text.includes("didn't accept")) fail("the printer refused the sudo password");
          await sudo();
        } else if (text.includes("Getting the install ready")) {
          await shot("getting-ready");
        } else if (await find("//section//div[contains(@class,'bar')]").then((b) => b.length > 0)) {
          await shot(`progress-${slug(heading)}`);
        } else {
          await shot(`stopped-at-${slug(text)}`);
          fail(`a screen this script does not answer:\n${text}`);
        }
      }
      await sleep(250);
    }
    log(`outcome: ${outcome}`);
    if (outcome !== "Nothing to update" && outcome !== "HelixScreen is installed") process.exitCode = 2;
  } finally {
    await wd("DELETE", s("")).catch(() => {});
  }
}

main().catch((e) => fail(e.stack ?? String(e)));
