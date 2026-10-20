// SPDX-License-Identifier: GPL-3.0-or-later
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig(({ mode }) => ({
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  // The mock layer answers every command and reads the staging printer list and its images
  // from the repo's manifest.
  ...(mode === "mock"
    ? {
        publicDir: "../manifest",
        resolve: {
          alias: [{ find: /^@tauri-apps\/api\/core$/, replacement: "/src/lib/mock-core.ts" }],
        },
        // Pre-bundled, the opener plugin would keep the real `invoke`.
        optimizeDeps: { exclude: ["@tauri-apps/plugin-opener"] },
      }
    : {}),
}));
