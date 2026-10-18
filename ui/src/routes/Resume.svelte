<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Shown above the printers while installs saved on this computer did not finish: every one of
     them, the ones that already changed a printer first. Those can only be continued; one that
     changed nothing can also be put aside for good. -->
<script lang="ts">
  import { ApiError } from "../lib/api";
  import { resumeOrder } from "../lib/runs";
  import type { RunSummary } from "../lib/types";

  let {
    runs,
    oncontinue,
    onforget,
    onhide,
  }: {
    runs: RunSummary[];
    oncontinue: (runId: string) => void;
    /** Ends a run that changed nothing, so it is not offered again. */
    onforget: (runId: string) => Promise<void>;
    /** Hides the banner until the user comes back to the printers. */
    onhide: () => void;
  } = $props();

  const ordered = $derived(resumeOrder(runs));
  let busy = $state<string | null>(null);
  let error = $state<ApiError | null>(null);

  function where(r: RunSummary): string {
    return r.total_steps ? ` It stopped at step ${r.step + 1} of ${r.total_steps}.` : "";
  }

  async function forget(runId: string) {
    busy = runId;
    error = null;
    try {
      await onforget(runId);
    } catch (e) {
      error = e as ApiError;
    } finally {
      busy = null;
    }
  }
</script>

<aside class="banner" class:committed={ordered[0]?.committed} role="status">
  <ul>
    {#each ordered as r, i (r.run_id)}
      <li class:committed={r.committed}>
        <div class="text">
          {#if r.committed}
            <strong>Your {r.printer_name} is part-way through an install.</strong>
            <span>Finish it before you use the printer.{where(r)}</span>
          {:else}
            <strong>Your install on {r.printer_name} didn't finish.</strong>
            <span>Nothing on the printer was changed yet.{where(r)}</span>
          {/if}
        </div>
        <div class="buttons">
          <button class:primary={i === 0} disabled={busy !== null} onclick={() => oncontinue(r.run_id)}>
            Continue the install
          </button>
          {#if !r.committed}
            <button disabled={busy !== null} onclick={() => forget(r.run_id)}>Don't continue</button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
  {#if error}<p class="error" role="alert">{error.message}</p>{/if}
  {#if !ordered.some((r) => r.committed)}
    <button class="link hide" onclick={onhide}>Not now</button>
  {/if}
</aside>

<style>
  .banner {
    padding: 0.5rem 1.25rem;
    border-radius: var(--radius);
    background: var(--accent-soft);
    border: 1px solid var(--border);
    margin-bottom: 1.5rem;
  }
  .banner.committed {
    background: var(--warn-soft);
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  li {
    display: flex;
    gap: 1rem;
    align-items: center;
    justify-content: space-between;
    padding: 0.5rem 0;
  }
  li + li {
    border-top: 1px solid var(--border);
  }
  .text {
    display: grid;
  }
  .buttons {
    display: flex;
    gap: 0.5rem;
    flex: none;
  }
  .error {
    color: var(--danger);
    font-weight: 600;
    margin: 0.25rem 0;
  }
  .hide {
    margin-bottom: 0.25rem;
  }
</style>
