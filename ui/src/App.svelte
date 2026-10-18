<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import * as api from "./lib/api";
  import { ApiError } from "./lib/api";
  import { session } from "./lib/session.svelte";
  import type { RunSummary } from "./lib/types";
  import Picker from "./routes/Picker.svelte";
  import Resume from "./routes/Resume.svelte";
  import Run from "./routes/Run.svelte";

  let screen = $state<"loading" | "no_list" | "picker" | "run">("loading");
  let runId = $state<string | null>(null);
  let listError = $state<ApiError | null>(null);
  let unfinished = $state<RunSummary[]>([]);
  let resumeHidden = $state(false);
  let versionClicks = 0;
  let newVersion = $state<string | null>(null);
  let updating = $state(false);

  async function installUpdate() {
    updating = true;
    try {
      await api.installUpdate();
    } catch {
      updating = false;
    }
  }

  async function load() {
    screen = "loading";
    listError = null;
    try {
      session.printers = await api.printers();
    } catch (e) {
      listError = e as ApiError;
      screen = "no_list";
      return;
    }
    await loadUnfinished();
    screen = "picker";
  }

  async function loadUnfinished() {
    try {
      unfinished = await api.unfinishedRuns();
    } catch {
      unfinished = [];
    }
  }

  function open(id: string) {
    runId = id;
    screen = "run";
  }

  /** Back from a run: what it left unfinished, and any other run, is offered again. */
  async function backToPicker() {
    resumeHidden = false;
    await loadUnfinished();
    screen = "picker";
  }

  async function forget(id: string) {
    await api.dismissRun(id);
    await loadUnfinished();
  }

  function versionClicked() {
    versionClicks += 1;
    if (versionClicks >= 5) {
      versionClicks = 0;
      session.recordMode = !session.recordMode;
    }
  }

  onMount(async () => {
    // Fired only when a release is newer; absent in a build without an updater key.
    listen<string>("update-available", (e) => (newVersion = e.payload)).catch(() => {});
    // The check may have finished before the listener was registered.
    api
      .pendingUpdate()
      .then((v) => {
        if (v) newVersion = v;
      })
      .catch(() => {});
    try {
      const info = await api.appInfo();
      session.version = info.version;
      session.imageBase = info.image_base;
    } catch {
      session.version = "";
    }
    await load();
  });
</script>

<div class="shell">
  <header class="top">
    <span class="logo">HelixScreen Installer</span>
  {#if newVersion}
    <div class="update" role="status">
      Version {newVersion} is available.
      <button class="primary" onclick={installUpdate} disabled={updating}>
        {updating ? "Updating…" : "Update and restart"}
      </button>
    </div>
  {/if}
  </header>

  <main>
    {#if screen === "loading"}
      <p class="muted">Loading the list of printers…</p>
    {:else if screen === "no_list"}
      <section class="card body">
        <h1>Can't load the printer list</h1>
        <p class="lead">
          The installer needs its list of supported printers before it can start, and it couldn't get a copy it
          can trust.
        </p>
        <p>Check that this computer is connected to the internet, then try again.</p>
        <div class="actions">
          <button class="primary" onclick={load}>Try again</button>
        </div>
        {#if listError?.detail}<p class="muted small">Technical detail: {listError.detail}</p>{/if}
      </section>
    {:else if screen === "picker"}
      {#if unfinished.length > 0 && !resumeHidden}
        <Resume runs={unfinished} oncontinue={open} onforget={forget} onhide={() => (resumeHidden = true)} />
      {/if}
      <Picker onstart={open} />
    {:else if screen === "run" && runId}
      {#key runId}
        <Run {runId} onexit={backToPicker} />
      {/key}
    {/if}
  </main>

  <footer>
    <button class="version" onclick={versionClicked} aria-label="Installer version">
      {session.version ? `Version ${session.version}` : ""}
    </button>
    {#if session.recordMode}<span class="tag">Record mode</span>{/if}
  </footer>
</div>

<style>
  .shell {
    min-height: 100%;
    display: grid;
    grid-template-rows: auto 1fr auto;
  }
  .top {
    padding: 1rem 2rem;
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }
  .logo {
    font-weight: 700;
    letter-spacing: 0.01em;
  }
  .update {
    display: flex;
    gap: 1rem;
    align-items: center;
    justify-content: center;
    margin-top: 0.5rem;
  }
  main {
    width: 100%;
    max-width: 880px;
    margin: 0 auto;
    padding: 2rem;
  }
  .body {
    padding: 2rem;
  }
  .lead {
    font-size: 1.1rem;
  }
  .small {
    font-size: 0.85rem;
    margin-top: 1.25rem;
  }
  footer {
    padding: 0.5rem 2rem;
    display: flex;
    gap: 0.75rem;
    align-items: center;
  }
  .version {
    border: none;
    background: none;
    color: var(--text-muted);
    font-weight: 400;
    font-size: 0.8rem;
    padding: 0.25rem 0;
    cursor: default;
  }
  .version:hover:not(:disabled) {
    background: none;
  }
  .tag {
    font-size: 0.75rem;
    font-weight: 700;
    color: var(--warn);
  }
</style>
