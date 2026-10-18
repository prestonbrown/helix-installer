<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- A printer's photo, or a drawn printer when it has none or the photo will not load. -->
<script lang="ts">
  import { imageUrl } from "./text";
  import { session } from "./session.svelte";

  let { photo, name, size = 88 }: { photo: string | null; name: string; size?: number } = $props();
  let broken = $state(false);
  const url = $derived(imageUrl(photo, session.imageBase));
  $effect(() => {
    url;
    broken = false;
  });
</script>

<div class="tile" style:width="{size}px" style:height="{size}px">
  {#if url && !broken}
    <img src={url} alt={name} onerror={() => (broken = true)} />
  {:else}
    <svg viewBox="0 0 48 48" aria-hidden="true">
      <rect x="7" y="6" width="34" height="36" rx="4" />
      <rect x="12" y="11" width="24" height="16" rx="2" />
      <line x1="24" y1="27" x2="24" y2="33" />
      <rect x="15" y="35" width="18" height="3" rx="1.5" />
    </svg>
  {/if}
</div>

<style>
  .tile {
    flex: none;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    display: grid;
    place-items: center;
    overflow: hidden;
  }
  img {
    width: 100%;
    height: 100%;
    object-fit: contain;
  }
  svg {
    width: 60%;
    height: 60%;
    fill: none;
    stroke: var(--text-muted);
    stroke-width: 2;
  }
</style>
