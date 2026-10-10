<script lang="ts">
  // Folder text field + Browse… button.
  import { pickFolder } from './lib/api';

  let { id, value = $bindable(), placeholder = '' }: { id: string; value: string; placeholder?: string } = $props();

  async function browse() {
    const p = await pickFolder();
    if (p) value = p;
  }
</script>

<div class="row">
  <input type="text" {id} bind:value {placeholder} />
  <button class="btn" onclick={browse}>Browse…</button>
</div>
