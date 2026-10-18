<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Choosing the printer: the ones found on the network, or a model and address typed by hand. -->
<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "../lib/api";
  import { ApiError } from "../lib/api";
  import Copyable from "../lib/Copyable.svelte";
  import PrinterTile from "../lib/PrinterTile.svelte";
  import { openGuide } from "../lib/open";
  import { session } from "../lib/session.svelte";
  import type { CandidateView, PrinterView } from "../lib/types";

  let { onstart }: { onstart: (runId: string) => void } = $props();

  let mode = $state<"list" | "manual">("list");
  let searching = $state(true);
  let found = $state<CandidateView[]>([]);
  let error = $state<ApiError | null>(null);
  let busy = $state<string | null>(null);
  let recording = $state<{ name: string; json: string } | null>(null);

  let model = $state<string | null>(null);
  let address = $state("");
  let addressError = $state<string | null>(null);

  async function search() {
    searching = true;
    error = null;
    try {
      found = await api.discover();
    } catch (e) {
      error = e as ApiError;
    } finally {
      searching = false;
    }
  }

  async function start(key: string, printerId: string | null = null) {
    busy = key;
    error = null;
    try {
      onstart(await api.startRun(key, printerId));
    } catch (e) {
      error = e as ApiError;
    } finally {
      busy = null;
    }
  }

  async function startManual() {
    if (!model) return;
    busy = "manual";
    error = null;
    addressError = null;
    try {
      const c = await api.addManualHost(address.trim());
      onstart(await api.startRun(c.key, model));
    } catch (e) {
      const err = e as ApiError;
      if (err.kind === "not_an_address") addressError = err.message;
      else error = err;
    } finally {
      busy = null;
    }
  }

  async function recordPrinter(c: CandidateView) {
    try {
      recording = { name: c.name, json: await api.recordPrinter(c.key) };
    } catch (e) {
      error = e as ApiError;
    }
  }

  function verifiedLine(p: PrinterView): string | null {
    if (!p.verified) return null;
    if (p.verified.by === "helixscreen") {
      return p.verified.firmware
        ? `Tested by HelixScreen on firmware ${p.verified.firmware}`
        : "Tested by HelixScreen";
    }
    return "Tested by other owners";
  }

  onMount(search);
</script>

{#snippet failure(e: ApiError)}
  <div class="notice danger" role="alert">
    <p>{e.message}</p>
    {#if e.doc}
      <button class="link" onclick={() => openGuide(e.doc!)}>Open the install guide</button>
    {/if}
  </div>
{/snippet}

{#if mode === "list"}
  <header class="head">
    <h1>Choose your printer</h1>
    <p class="muted">Make sure your printer is switched on and on the same network as this computer.</p>
  </header>

  {#if error}{@render failure(error)}{/if}

  {#if searching}
    <div class="searching card">
      <div class="spinner" aria-hidden="true"></div>
      <div>
        <h2>Looking for printers on your network</h2>
        <p class="muted">This takes a few seconds.</p>
      </div>
    </div>
  {:else if found.length === 0}
    <div class="card empty">
      <h2>No printers found</h2>
      <p class="muted">
        Check that the printer is switched on and connected to your Wi-Fi or network cable, then search
        again. A printer that isn't connected to a network yet won't show up here.
      </p>
    </div>
  {:else}
    <ul class="found">
      {#each found as c (c.key)}
        <li class="card candidate">
          <PrinterTile photo={c.photo} name={c.name} />
          <div class="about">
            <h2>{c.name}</h2>
            <p class="muted address">At {c.address}</p>
            {#if c.status === "ready"}
              <p class="status ok">
                {c.helix_version
                  ? `HelixScreen ${c.helix_version} is installed`
                  : c.helix_version === ""
                    ? "HelixScreen is installed"
                    : "Ready to install"}
              </p>
            {:else if c.status === "choose"}
              <p class="status">Which model is this?</p>
              <div class="choices">
                {#each c.choices as p (p.id)}
                  <button disabled={busy !== null} onclick={() => start(c.key, p.id)}>{p.name}</button>
                {/each}
              </div>
            {:else if c.status === "unsupported"}
              <p class="status warn">The installer can't set up this printer yet.</p>
            {:else}
              <p class="status warn">This printer didn't answer.</p>
            {/if}
            {#if session.recordMode}
              <button class="link" onclick={() => recordPrinter(c)}>Record this printer</button>
            {/if}
          </div>
          <div class="go">
            {#if c.status === "ready"}
              <button class="primary" disabled={busy !== null} onclick={() => start(c.key)}>
                {c.helix_version !== null ? "Update HelixScreen" : "Install HelixScreen"}
              </button>
            {:else if c.status === "unsupported" && c.doc}
              <button onclick={() => openGuide(c.doc!)}>Read the install guide</button>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}

  {#if recording}
    <section class="card recording">
      <h2>Recording of {recording.name}</h2>
      <p class="muted">Addresses and the serial number are hidden. Copy this into a new probe fixture.</p>
      <Copyable text={recording.json} rows={6} />
      <button class="link" onclick={() => (recording = null)}>Close</button>
    </section>
  {/if}

  <div class="actions">
    <button disabled={searching} onclick={search}>Search again</button>
    <button class="link" onclick={() => (mode = "manual")}>My printer isn't listed</button>
  </div>
{:else}
  <header class="head">
    <h1>Find your printer</h1>
    <p class="muted">Pick your printer's model, then type its address.</p>
  </header>

  {#if error}{@render failure(error)}{/if}

  <h2>1. Your printer's model</h2>
  <div class="models" role="radiogroup" aria-label="Printer model">
    {#each session.printers as p (p.id)}
      <button
        class="card model"
        class:selected={model === p.id}
        role="radio"
        aria-checked={model === p.id}
        onclick={() => (model = p.id)}
      >
        <PrinterTile photo={p.photo} name={p.name} size={72} />
        <span class="model-name">{p.name}</span>
        {#if verifiedLine(p)}<span class="muted small">{verifiedLine(p)}</span>{/if}
      </button>
    {/each}
  </div>

  <h2 class="second">2. Your printer's address</h2>
  <label class="field address-field">
    <span class="visually-hidden">Printer address</span>
    <input type="text" inputmode="decimal" placeholder="192.168.1.50" bind:value={address} />
  </label>
  {#if addressError}<p class="field-error" role="alert">{addressError}</p>{/if}
  <p class="muted small">
    Your printer shows its address on its screen, usually under Settings, then Network or Wi-Fi. It is four
    numbers with dots between them.
  </p>

  <div class="actions">
    <button
      class="primary"
      disabled={!model || !address.trim() || busy !== null}
      onclick={startManual}
    >
      Continue
    </button>
    <button class="link" onclick={() => (mode = "list")}>Back to the printers found</button>
  </div>
{/if}

<style>
  .head {
    margin-bottom: 1.5rem;
  }
  .searching,
  .empty {
    padding: 1.5rem;
    display: flex;
    gap: 1.25rem;
    align-items: center;
  }
  .empty {
    display: block;
  }
  .spinner {
    width: 36px;
    height: 36px;
    border-radius: 50%;
    border: 4px solid var(--surface-2);
    border-top-color: var(--accent);
    animation: spin 0.9s linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
  .found {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 0.75rem;
  }
  .candidate {
    display: flex;
    gap: 1.25rem;
    align-items: center;
    padding: 1rem 1.25rem;
  }
  .about {
    flex: 1;
    min-width: 0;
  }
  .about h2 {
    margin: 0;
  }
  .address {
    margin: 0 0 0.25rem;
    font-size: 0.9rem;
  }
  .status {
    margin: 0;
    font-weight: 600;
  }
  .status.ok {
    color: var(--ok);
  }
  .status.warn {
    color: var(--warn);
  }
  .choices {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
    flex-wrap: wrap;
  }
  .choices button {
    padding: 0.4rem 0.9rem;
  }
  .go {
    flex: none;
  }
  .recording {
    margin-top: 1rem;
    padding: 1.25rem;
  }
  .models {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(170px, 1fr));
    gap: 0.75rem;
  }
  .model {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.5rem;
    padding: 1rem 0.75rem;
    text-align: center;
    font-weight: 400;
  }
  .model.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent);
    background: var(--accent-soft);
  }
  .model-name {
    font-weight: 600;
  }
  .small {
    font-size: 0.85rem;
  }
  .second {
    margin-top: 1.75rem;
  }
  .address-field {
    max-width: 320px;
    margin-bottom: 0.5rem;
  }
  .field-error {
    color: var(--danger);
    font-weight: 600;
  }
  .visually-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
  }
</style>
