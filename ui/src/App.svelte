<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  let version = $state("");
  invoke<string>("core_version").then((v) => (version = v));
</script>

<main>
  <h1>HelixScreen Installer</h1>
  <p>Core {version}</p>
</main>
