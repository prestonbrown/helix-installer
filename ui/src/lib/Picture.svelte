<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- An image from the printer list. One that fails to load takes no space. -->
<script lang="ts">
  import { imageUrl } from "./text";
  import { session } from "./session.svelte";

  let { src, alt = "", class: cls = "" }: { src: string | null | undefined; alt?: string; class?: string } =
    $props();
  let broken = $state(false);
  const url = $derived(imageUrl(src, session.imageBase));
  $effect(() => {
    url;
    broken = false;
  });
</script>

{#if url && !broken}
  <img class={cls} src={url} {alt} onerror={() => (broken = true)} />
{/if}
