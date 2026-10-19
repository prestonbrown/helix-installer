<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- One install, one screen per thing the installer needs from the user. -->
<script lang="ts">
  import { onMount } from "svelte";
  import * as api from "../lib/api";
  import { ApiError } from "../lib/api";
  import Picture from "../lib/Picture.svelte";
  import PrinterTile from "../lib/PrinterTile.svelte";
  import { openGuide } from "../lib/open";
  import { printerName, session } from "../lib/session.svelte";
  import { signInInput, sudoInput } from "../lib/login";
  import { formatBytes, isUrl, keepPowerOn, keepPowerText, stepLabel } from "../lib/text";
  import type { AdvanceView, Input } from "../lib/types";
  import Recovery from "./Recovery.svelte";

  let { runId, onexit }: { runId: string; onexit: () => void } = $props();

  let view = $state<AdvanceView | null>(null);
  let busy = $state(false);
  let error = $state<ApiError | null>(null);
  let alive = true;

  let user = $state("");
  let password = $state("");
  let showPassword = $state(false);
  let useKey = $state(false);
  let passphrase = $state("");
  let sudoPassword = $state("");
  let loginRefused = $state(false);
  let drive = $state<string | null>(null);

  const ev = $derived(view?.event ?? null);
  const run = $derived(view?.run ?? null);

  /** Sends the answer, then keeps asking while the engine reports progress. */
  async function send(input: Input | null = null) {
    busy = true;
    error = null;
    const sentLogin = input?.input === "login";
    try {
      let v = await api.advance(runId, input);
      while (alive) {
        view = v;
        if (v.event.event !== "progress") break;
        v = await api.advance(runId);
      }
      loginRefused = sentLogin && v.event.event === "need_input" && v.event.prompt === "login";
      if (v.event.event === "need_input" && v.event.prompt === "login" && !user) {
        user = v.event.user_hint ?? "";
      }
    } catch (e) {
      error = e as ApiError;
    } finally {
      busy = false;
    }
  }

  function signIn(e: SubmitEvent) {
    e.preventDefault();
    const input = signInInput({ user, password, useKey, passphrase });
    password = "";
    passphrase = "";
    send(input);
  }

  function giveSudoPassword(e: SubmitEvent) {
    e.preventDefault();
    const input = sudoInput(sudoPassword);
    sudoPassword = "";
    send(input);
  }

  function paragraphs(text: string): string[] {
    return text.split(/\n\s*\n/).filter((p) => p.trim());
  }

  function sentence(s: string): string {
    return s ? s[0].toUpperCase() + s.slice(1).replace(/([^.!?])$/, "$1.") : s;
  }

  onMount(() => {
    send();
    return () => (alive = false);
  });
</script>

{#snippet stopLink()}
  {#if run && !run.committed}
    <button class="link" disabled={busy} onclick={() => send({ input: "cancel" })}>Stop the install</button>
  {/if}
{/snippet}

<div class="run">
  {#if run}
    <header class="strip">
      <div>
        <div class="printer">{run.printer_name}</div>
        {#if run.total_steps > 0 && ev?.event !== "done"}
          <div class="muted">
            Step {Math.min(run.step + 1, run.total_steps)} of {run.total_steps}: {stepLabel(run.step_kind)}
          </div>
        {/if}
      </div>
      {#if run.total_steps > 0}
        <div
          class="steps"
          role="progressbar"
          aria-valuemin="0"
          aria-valuemax={run.total_steps}
          aria-valuenow={ev?.event === "done" ? run.total_steps : run.step}
        >
          {#each { length: run.total_steps } as _, i}
            <span class:done={ev?.event === "done" || i < run.step} class:now={i === run.step}></span>
          {/each}
        </div>
      {/if}
    </header>
  {/if}

  {#if error}
    <div class="notice danger" role="alert">
      <p>{error.message}</p>
      <div class="actions">
        <button class="primary" onclick={() => send()}>Try again</button>
        <button class="link" onclick={onexit}>Back to printers</button>
      </div>
    </div>
  {/if}

  {#if !ev}
    {#if !error}
      <div class="card body"><p class="muted">Getting the install ready…</p></div>
    {/if}
  {:else if ev.event === "need_input" && ev.prompt === "info"}
    <section class="card body">
      <Picture src={ev.image} class="shot" />
      {#each paragraphs(ev.text) as p}<p class="lead">{p}</p>{/each}
      <div class="actions">
        <button class="primary" disabled={busy} onclick={() => send({ input: "next" })}>Next</button>
        {@render stopLink()}
      </div>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "confirm"}
    <section class="card body">
      {#each paragraphs(ev.text) as p}<p class="lead">{p}</p>{/each}
      <div class="actions">
        <button class="primary" disabled={busy} onclick={() => send({ input: "next" })}>{ev.button}</button>
        {@render stopLink()}
      </div>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "user_action"}
    <section class="card body">
      <p class="eyebrow">On your printer</p>
      <Picture src={ev.image} class="shot" />
      {#each paragraphs(ev.text) as p}<p class="lead">{p}</p>{/each}
      <div class="actions">
        <button class="primary" disabled={busy} onclick={() => send({ input: "next" })}>I've done this</button>
        {@render stopLink()}
      </div>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "ask"}
    <section class="card body">
      <h1>{ev.question}</h1>
      <div class="answers">
        {#each ev.choices as c (c.value)}
          <button class="answer" disabled={busy} onclick={() => send({ input: "choice", value: c.value })}>
            <Picture src={c.image} class="answer-shot" />
            <span>{c.label}</span>
          </button>
        {/each}
      </div>
      <div class="actions">{@render stopLink()}</div>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "ambiguous"}
    <section class="card body">
      <h1>Which printer is this?</h1>
      <p class="muted">It answers like more than one model. Pick the one you have.</p>
      <div class="answers">
        {#each ev.candidates as id (id)}
          <button class="answer" disabled={busy} onclick={() => send({ input: "choice", value: id })}>
            <PrinterTile
              photo={session.printers.find((p) => p.id === id)?.photo ?? null}
              name={printerName(id)}
            />
            <span>{printerName(id)}</span>
          </button>
        {/each}
      </div>
      <div class="actions">{@render stopLink()}</div>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "login"}
    <section class="card body narrow">
      <h1>Sign in to your printer</h1>
      <p>
        The installer needs your printer's user name and password, or your SSH key, to install HelixScreen. It
        uses them only for this install and never saves them.
      </p>
      {#if ev.password_hint && !useKey}
        <p>The password is {ev.password_hint}.</p>
      {/if}
      {#if loginRefused}
        <div class="notice danger" role="alert">
          {useKey
            ? "The printer didn't accept your SSH key for that user name. Check the name, or sign in with a password."
            : "The printer didn't accept that password. Check it and try again."}
        </div>
      {/if}
      <form onsubmit={signIn}>
        <label class="field">
          <span>User name</span>
          <input type="text" autocomplete="off" autocapitalize="off" spellcheck="false" bind:value={user} />
        </label>
        {#if ev.has_ssh_key}
          <label class="check">
            <input type="checkbox" bind:checked={useKey} /> Use my SSH key instead of a password
          </label>
        {/if}
        {#if useKey && ev.has_ssh_key}
          {#if ev.key_locked}
            <label class="field">
              <span>Key passphrase</span>
              <input
                type={showPassword ? "text" : "password"}
                autocomplete="off"
                spellcheck="false"
                bind:value={passphrase}
              />
            </label>
            <p class="muted">The passphrase you chose when you made the key. Leave it empty if your key has none.</p>
          {/if}
        {:else}
          <label class="field">
            <span>Password</span>
            <input
              type={showPassword ? "text" : "password"}
              autocomplete="off"
              spellcheck="false"
              bind:value={password}
            />
          </label>
        {/if}
        {#if !useKey || ev.key_locked}
          <label class="check">
            <input type="checkbox" bind:checked={showPassword} />
            {useKey ? "Show the passphrase" : "Show the password"}
          </label>
        {/if}
        <div class="actions">
          <button class="primary" type="submit" disabled={busy || !user.trim()}>Sign in</button>
          {@render stopLink()}
        </div>
      </form>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "sudo_password"}
    <section class="card body narrow">
      <h1>Your printer needs your password</h1>
      <p>
        You signed in with your SSH key. To install HelixScreen the printer also needs the password of the {ev.user}
        account. The installer uses it only for this install and never saves it.
      </p>
      {#if ev.refused}
        <div class="notice danger" role="alert">The printer didn't accept that password. Check it and try again.</div>
      {/if}
      <form onsubmit={giveSudoPassword}>
        <label class="field">
          <span>Password for {ev.user}</span>
          <input
            type={showPassword ? "text" : "password"}
            autocomplete="off"
            spellcheck="false"
            bind:value={sudoPassword}
          />
        </label>
        <label class="check">
          <input type="checkbox" bind:checked={showPassword} /> Show the password
        </label>
        <div class="actions">
          <button class="primary" type="submit" disabled={busy || !sudoPassword}>Continue</button>
          {@render stopLink()}
        </div>
      </form>
    </section>
  {:else if ev.event === "need_input" && ev.prompt === "choose_drive"}
    <section class="card body">
      <h1>Choose a USB stick</h1>
      <p>
        The installer copies {formatBytes(ev.needed_bytes)} of files onto the stick. The stick must be formatted
        as FAT32. Other files on it are left alone.
      </p>
      {#if ev.drives.length === 0}
        <div class="notice warn" role="status">{ev.message ?? "No USB stick found. Plug one in."}</div>
        <div class="actions">
          <button class="primary" disabled={busy} onclick={() => send()}>Check again</button>
          {@render stopLink()}
        </div>
      {:else}
        <div class="drives" role="radiogroup" aria-label="USB sticks">
          {#each ev.drives as d (d.id)}
            <button
              class="drive"
              class:selected={drive === d.id}
              role="radio"
              aria-checked={drive === d.id}
              onclick={() => (drive = d.id)}
            >
              <svg viewBox="0 0 24 24" aria-hidden="true"
                ><rect x="7" y="2" width="10" height="7" rx="1" /><rect x="5" y="9" width="14" height="13" rx="2" /></svg
              >
              <span class="drive-name">{d.label}</span>
              <span class="muted">{formatBytes(d.size_bytes)} stick, {formatBytes(d.free_bytes)} free</span>
            </button>
          {/each}
        </div>
        <div class="actions">
          <button
            class="primary"
            disabled={busy || !drive}
            onclick={() => drive && send({ input: "drive", value: drive })}>Use this stick</button
          >
          <button disabled={busy} onclick={() => send()}>Look for sticks again</button>
          {@render stopLink()}
        </div>
      {/if}
    </section>
  {:else if ev.event === "progress"}
    <section class="card body">
      <h1>{ev.title}</h1>
      {#if ev.detail}<p class="muted">{ev.detail}</p>{/if}
      <div class="bar" class:indeterminate={ev.fraction === null}>
        <span style:width={ev.fraction === null ? "35%" : `${Math.round(ev.fraction * 100)}%`}></span>
      </div>
      {#if keepPowerOn(run?.step_kind ?? null, run?.committed ?? false)}
        <div class="notice warn" role="status">
          {keepPowerText(run?.step_kind ?? null)}
        </div>
      {/if}
    </section>
  {:else if ev.event === "done"}
    <section class="card body done">
      <div class="badge ok" aria-hidden="true">
        <svg viewBox="0 0 24 24"><path d="M5 12.5l4.5 4.5L19 7.5" /></svg>
      </div>
      {#if run?.done_note}
        <h1>Nothing to update</h1>
        <p class="lead">{sentence(run.done_note)}</p>
        <p>Nothing on your printer was changed.</p>
      {:else}
        <h1>HelixScreen is installed</h1>
        <p class="lead">
          Your {run?.printer_name} may restart on its own. When it has, its screen shows HelixScreen.
        </p>
      {/if}
      <div class="actions">
        <button class="primary" onclick={onexit}>Finish</button>
      </div>
    </section>
  {:else if ev.event === "refused"}
    <section class="card body">
      {#if ev.cancelled}
        <h1>Install stopped</h1>
        <p class="lead">Nothing on your printer was changed.</p>
      {:else}
        <h1>This printer can't be set up automatically</h1>
        <p class="lead">Nothing on your printer was changed.</p>
        <p>
          The installer stops whenever it isn't sure it has the right files for your printer. The install guide
          shows how to do it by hand.
        </p>
        <p class="muted">Why it stopped: {sentence(ev.reason)}</p>
      {/if}
      <div class="actions">
        <button class="primary" onclick={() => openGuide(ev.doc)}>Open the install guide</button>
        <button onclick={onexit}>Back to printers</button>
      </div>
    </section>
  {:else if ev.event === "failed" && !ev.committed}
    <section class="card body">
      <h1>The install stopped</h1>
      <p class="lead">Nothing on your printer was changed.</p>
      <p class="muted">What happened: {sentence(ev.reason)}</p>
      {#if ev.help && !isUrl(ev.help)}<p class="help-text">{ev.help}</p>{/if}
      <div class="actions">
        <button class="primary" disabled={busy} onclick={() => send()}>Try again</button>
        <button onclick={() => openGuide(isUrl(ev.help) ? ev.help : (run?.guide ?? ""))}>
          Open the install guide
        </button>
        <button class="link" onclick={onexit}>Back to printers</button>
      </div>
    </section>
  {:else if ev.event === "failed" && run}
    <Recovery event={ev} {run} {busy} onretry={() => send()} {onexit} />
  {/if}
</div>

<style>
  .run {
    display: grid;
    gap: 1rem;
  }
  .strip {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 1.5rem;
  }
  .printer {
    font-weight: 700;
    font-size: 1.1rem;
  }
  .steps {
    display: flex;
    gap: 4px;
    flex: 0 1 320px;
  }
  .steps span {
    flex: 1;
    height: 6px;
    border-radius: 3px;
    background: var(--border);
  }
  .steps span.done {
    background: var(--accent);
  }
  .steps span.now {
    background: var(--accent);
    opacity: 0.45;
  }
  .body {
    padding: 2rem;
  }
  .narrow {
    max-width: 560px;
  }
  .lead {
    font-size: 1.1rem;
  }
  .eyebrow {
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 0.8rem;
    font-weight: 700;
    color: var(--accent);
  }
  .body :global(.shot) {
    display: block;
    max-width: 100%;
    max-height: 300px;
    border-radius: var(--radius-sm);
    margin-bottom: 1.25rem;
    border: 1px solid var(--border);
  }
  .answers {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(190px, 1fr));
    gap: 0.75rem;
    margin-top: 1rem;
  }
  .answer {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.6rem;
    padding: 1rem;
    text-align: center;
  }
  .answer :global(.answer-shot) {
    max-width: 100%;
    max-height: 140px;
    border-radius: 6px;
  }
  .check {
    display: flex;
    gap: 0.5rem;
    align-items: center;
  }
  .drives {
    display: grid;
    gap: 0.5rem;
    margin-top: 1rem;
  }
  .drive {
    display: grid;
    grid-template-columns: 32px 1fr auto;
    align-items: center;
    gap: 0.75rem;
    text-align: left;
    font-weight: 400;
  }
  .drive svg {
    width: 28px;
    height: 28px;
    fill: none;
    stroke: var(--text-muted);
    stroke-width: 1.6;
  }
  .drive.selected {
    border-color: var(--accent);
    box-shadow: 0 0 0 2px var(--accent);
    background: var(--accent-soft);
  }
  .drive-name {
    font-weight: 600;
  }
  .bar {
    height: 10px;
    border-radius: 5px;
    background: var(--surface-2);
    overflow: hidden;
    margin: 1.25rem 0;
  }
  .bar span {
    display: block;
    height: 100%;
    background: var(--accent);
    border-radius: 5px;
    transition: width 0.3s;
  }
  .bar.indeterminate span {
    animation: slide 1.4s ease-in-out infinite;
  }
  @keyframes slide {
    from {
      transform: translateX(-100%);
    }
    to {
      transform: translateX(300%);
    }
  }
  .badge {
    width: 56px;
    height: 56px;
    border-radius: 50%;
    display: grid;
    place-items: center;
    margin-bottom: 1rem;
  }
  .badge.ok {
    background: var(--ok-soft);
  }
  .badge svg {
    width: 30px;
    height: 30px;
    fill: none;
    stroke: var(--ok);
    stroke-width: 2.5;
    stroke-linecap: round;
    stroke-linejoin: round;
  }
  /* Help can hold a command on its own line. */
  .help-text {
    white-space: pre-line;
  }
</style>
