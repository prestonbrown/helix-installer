// SPDX-License-Identifier: GPL-3.0-or-later
// Captures every screen headless, against the mock layer, in light and dark at the two window
// sizes the app is designed for. Usage: node scripts/shots.mjs <output dir>
// CHROMIUM=<path> picks the browser binary when Playwright's own revision is not installed.
import { mkdirSync, readdirSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { chromium } from "playwright";
import { createServer } from "vite";

const out = process.argv[2];
if (!out) {
  console.error("usage: node scripts/shots.mjs <output dir>");
  process.exit(2);
}
mkdirSync(out, { recursive: true });

function browserPath() {
  if (process.env.CHROMIUM) return process.env.CHROMIUM;
  const root = join(homedir(), ".cache/ms-playwright");
  if (!existsSync(root)) return undefined;
  const shell = readdirSync(root)
    .filter((d) => d.startsWith("chromium_headless_shell-"))
    .sort()
    .pop();
  return shell ? join(root, shell, "chrome-headless-shell-linux64/chrome-headless-shell") : undefined;
}

const install = async (page) => {
  await page.waitForSelector(".candidate");
  await page.getByRole("button", { name: "Install HelixScreen" }).first().click();
};

const shots = [
  ["picker", "picker", async (p) => p.waitForSelector(".candidate >> nth=2")],
  [
    "picker-not-listed",
    "picker",
    async (p) => {
      await p.waitForSelector(".candidate");
      await p.getByRole("button", { name: "My printer isn't listed" }).click();
      await p.getByRole("radio", { name: /Creality K1/ }).click();
      await p.getByPlaceholder("192.168.1.50").fill("192.168.1.50");
    },
  ],
  ["confirm", "confirm", async (p) => (await install(p), p.getByRole("button", { name: "Continue" }).waitFor())],
  ["warn", "warn", async (p) => (await install(p), p.getByRole("button", { name: "Continue anyway" }).waitFor())],
  ["info", "info", async (p) => (await install(p), p.getByRole("button", { name: "Next" }).waitFor())],
  ["user-action", "user_action", async (p) => (await install(p), p.getByRole("button", { name: "I've done this" }).waitFor())],
  ["ask", "ask", async (p) => (await install(p), p.waitForSelector(".answer"))],
  ["login", "login", async (p) => (await install(p), p.getByText("Sign in to your printer").waitFor())],
  [
    "login-key",
    "login",
    async (p) => {
      await install(p);
      await p.getByText("Use my SSH key instead of a password").click();
      await p.getByText("Key passphrase").waitFor();
    },
  ],
  ["sudo-password", "sudo", async (p) => (await install(p), p.getByText("Your printer needs your password").waitFor())],
  ["choose-drive", "drive", async (p) => (await install(p), p.waitForSelector(".drive"))],
  ["choose-drive-empty", "drive_empty", async (p) => (await install(p), p.getByText("Check again").waitFor())],
  ["progress", "progress", async (p) => (await install(p), p.waitForSelector(".bar"))],
  ["done", "done", async (p) => (await install(p), p.getByText("HelixScreen is installed").waitFor())],
  ["up-to-date", "up_to_date", async (p) => (await install(p), p.getByText("Nothing to update").waitFor())],
  ["refused", "refused", async (p) => (await install(p), p.getByText("can't be set up").waitFor())],
  ["failed-not-committed", "failed", async (p) => (await install(p), p.getByText("The install stopped").waitFor())],
  ["recovery", "recovery", async (p) => (await install(p), p.getByText("Get help").waitFor())],
  ["resume-banner", "resume", async (p) => (await p.waitForSelector(".banner"), p.waitForSelector(".candidate"))],
  ["no-printer-list", "no_list", async (p) => p.getByText("Can't load the printer list").waitFor()],
];

const server = await createServer({ mode: "mock", server: { port: 5199, strictPort: false }, logLevel: "error" });
await server.listen();
const base = server.resolvedUrls.local[0];
const browser = await chromium.launch({ headless: true, executablePath: browserPath() });
const written = [];
try {
  for (const [w, h] of [
    [1280, 800],
    [1024, 640],
  ]) {
    for (const theme of ["light", "dark"]) {
      const ctx = await browser.newContext({ viewport: { width: w, height: h }, colorScheme: theme });
      for (const [name, screen, ready] of shots) {
        const page = await ctx.newPage();
        await page.goto(`${base}?screen=${screen}`);
        await ready(page);
        await page.mouse.move(0, 0);
        await page.waitForTimeout(250);
        const file = join(out, `${name}-${w}x${h}-${theme}.png`);
        await page.screenshot({ path: file, fullPage: true, animations: "disabled" });
        written.push(file);
        await page.close();
      }
      await ctx.close();
    }
  }
} finally {
  await browser.close();
  await server.close();
}
console.log(written.join("\n"));
