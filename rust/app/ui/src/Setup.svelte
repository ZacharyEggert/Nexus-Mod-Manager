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
  // svelte-ignore state_referenced_locally
  let launch = $state(game.launch || '');
  // svelte-ignore state_referenced_locally
  let toolFolder = $state(game.tool_folder || '');
  // svelte-ignore state_referenced_locally
  let launchArgs = $state(game.launch_args || '');

  async function save() {
    await run('Saving settings', () =>
      api.configure(game.mode_id, gamePath, modsDir || null, launch.trim() || null, toolFolder || null, launchArgs.trim() || null),
    );
    onsaved();
  }
</script>

<div class="setup">
  <strong>Set up {game.name}</strong>
  <label for="gp">Game folder</label>
  <PathField id="gp" bind:value={gamePath} />
  <label for="md">Mods folder (same drive as the game, so files can be hard-linked)</label>
  <PathField id="md" bind:value={modsDir} placeholder="Default (see App settings)" />
  {#if game.required_tool}
    <label for="tf">{game.required_tool} folder (mods install into it)</label>
    <PathField id="tf" bind:value={toolFolder} />
  {/if}
  {#if game.allow_custom}
    <label for="lc">Custom launch command (in the game folder, or a full path)</label>
    <div class="row"><input type="text" id="lc" bind:value={launch} placeholder={game.default_launch ? `Default: ${game.default_launch}` : ''} /></div>
    <label for="la">Custom launch arguments</label>
    <div class="row"><input type="text" id="la" bind:value={launchArgs} /></div>
  {/if}
  <p><button class="btn primary" onclick={save}>Save</button></p>
</div>
