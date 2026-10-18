<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Shown on launch when an install saved on this computer did not finish. -->
<script lang="ts">
  import type { RunSummary } from "../lib/types";

  let {
    runs,
    oncontinue,
    ondismiss,
  }: { runs: RunSummary[]; oncontinue: (runId: string) => void; ondismiss: () => void } = $props();

  const latest = $derived(runs[runs.length - 1]);
  const where = $derived(
    latest.total_steps ? ` It stopped at step ${latest.step + 1} of ${latest.total_steps}.` : "",
  );
</script>

<aside class="banner" class:committed={latest.committed} role="status">
  <div class="text">
    {#if latest.committed}
      <strong>Your {latest.printer_name} is part-way through an install.</strong>
      <span>Finish it before you use the printer.{where}</span>
    {:else}
      <strong>Your install on {latest.printer_name} didn't finish.</strong>
      <span>Nothing on the printer was changed yet.{where}</span>
    {/if}
  </div>
  <div class="buttons">
    <button class="primary" onclick={() => oncontinue(latest.run_id)}>Continue the install</button>
    <button onclick={ondismiss}>Not now</button>
  </div>
</aside>

<style>
  .banner {
    display: flex;
    gap: 1rem;
    align-items: center;
    justify-content: space-between;
    padding: 1rem 1.25rem;
    border-radius: var(--radius);
    background: var(--accent-soft);
    border: 1px solid var(--border);
    margin-bottom: 1.5rem;
  }
  .banner.committed {
    background: var(--warn-soft);
  }
  .text {
    display: grid;
  }
  .buttons {
    display: flex;
    gap: 0.5rem;
    flex: none;
  }
</style>
