<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- The install stopped after it had changed the printer: what the printer is in now, what to do
     next, and a report the user can send for help. -->
<script lang="ts">
  import * as api from "../lib/api";
  import { ApiError } from "../lib/api";
  import Copyable from "../lib/Copyable.svelte";
  import { openGuide } from "../lib/open";
  import { isUrl, stepLabel, stoppedState } from "../lib/text";
  import type { RunEvent, RunInfo } from "../lib/types";

  let {
    event,
    run,
    busy,
    onretry,
    onexit,
  }: {
    event: Extract<RunEvent, { event: "failed" }>;
    run: RunInfo;
    busy: boolean;
    onretry: () => void;
    onexit: () => void;
  } = $props();

  let mask = $state(true);
  let report = $state<string | null>(null);
  let reportError = $state<ApiError | null>(null);

  const guide = $derived(isUrl(event.help) ? event.help : run.guide);

  async function makeReport() {
    reportError = null;
    try {
      report = await api.exportBundle(run.run_id, mask);
    } catch (e) {
      reportError = e as ApiError;
    }
  }

  $effect(() => {
    mask;
    report = null;
  });
</script>

<section class="card body">
  <div class="notice warn" role="status">Do not turn off your printer.</div>
  <h1>Your {run.printer_name} is part-way through the install</h1>

  <h2>Where your printer is now</h2>
  <p>
    The install stopped at step {Math.min(run.step + 1, run.total_steps)} of {run.total_steps}: {stepLabel(
      run.step_kind,
    )}.
  </p>
  <p>{stoppedState(run.step_kind)}</p>

  <h2>What to do next</h2>
  <ol>
    <li>Leave the printer switched on and connected to your network.</li>
    <li>Choose <strong>Try this step again</strong>.</li>
    <li>If it stops again, follow the recovery guide for your printer.</li>
  </ol>
  {#if event.help && !isUrl(event.help)}<p class="muted">{event.help}</p>{/if}

  <div class="actions">
    <button class="primary" disabled={busy} onclick={onretry}>Try this step again</button>
    <button onclick={() => openGuide(guide)}>Open the recovery guide</button>
    <button class="link" onclick={onexit}>Back to printers</button>
  </div>
</section>

<section class="card body help">
  <h2>Get help</h2>
  <p>
    Make a report of this install to send with your question. It lists each step and what the printer answered.
    Passwords are never included.
  </p>
  <label class="check">
    <input type="checkbox" bind:checked={mask} />
    Hide my network addresses and the printer's serial number
  </label>
  {#if reportError}<div class="notice danger" role="alert">{reportError.message}</div>{/if}
  {#if report === null}
    <div class="actions">
      <button onclick={makeReport}>Make the report</button>
    </div>
  {:else}
    <Copyable text={report} rows={8} />
  {/if}
  <p class="muted small">Technical detail: {event.reason}</p>
</section>

<style>
  .body {
    padding: 2rem;
  }
  h2 {
    margin-top: 1.5rem;
  }
  ol {
    margin: 0 0 0.75rem;
    padding-left: 1.25rem;
  }
  li {
    margin-bottom: 0.35rem;
  }
  .help h2 {
    margin-top: 0;
  }
  .check {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    margin: 0.5rem 0 0.75rem;
  }
  .small {
    font-size: 0.85rem;
    margin-top: 1rem;
  }
</style>
