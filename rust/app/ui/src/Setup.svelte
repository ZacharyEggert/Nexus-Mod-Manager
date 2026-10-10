<script lang="ts">
  // Per-game folders.
  import * as api from './lib/api';
  import { run } from './lib/ui.svelte';
  import PathField from './PathField.svelte';

  let { game, onsaved }: { game: api.GameInfo; onsaved: () => void } = $props();

  // Form fields start from the game's config; edits stay local until Save.
  // svelte-ignore state_referenced_locally
  let gamePath = $state(game.game_path || game.detected || '');
  // svelte-ignore state_referenced_locally
  let modsDir = $state(game.mods_dir || '');

  async function save() {
    await run('Saving settings', () => api.configure(game.mode_id, gamePath, modsDir || null));
    onsaved();
  }
</script>

<div class="setup">
  <strong>Set up {game.name}</strong>
  <label for="gp">Game folder</label>
  <PathField id="gp" bind:value={gamePath} />
  <label for="md">Mods folder (same drive as the game, so files can be hard-linked)</label>
  <PathField id="md" bind:value={modsDir} placeholder="Default (see App settings)" />
  <p><button class="btn primary" onclick={save}>Save</button></p>
</div>
