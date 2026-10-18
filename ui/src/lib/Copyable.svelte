<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<!-- Text the user may need to send someone, with a button that copies it. -->
<script lang="ts">
  let { text, rows = 10 }: { text: string; rows?: number } = $props();
  let copied = $state(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      copied = true;
      setTimeout(() => (copied = false), 2500);
    } catch {
      copied = false;
    }
  }
</script>

<div class="copyable">
  <textarea readonly {rows} value={text}></textarea>
  <div class="actions">
    <button class="primary" onclick={copy}>{copied ? "Copied" : "Copy to clipboard"}</button>
  </div>
</div>

<style>
  textarea {
    font-family: var(--mono);
    font-size: 0.85rem;
    resize: vertical;
    background: var(--surface-2);
  }
  .actions {
    margin-top: 0.75rem;
  }
</style>
