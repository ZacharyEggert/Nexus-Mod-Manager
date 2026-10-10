<script lang="ts">
  import * as api from './lib/api';
  import { LINK_MODES, run } from './lib/ui.svelte';
  import PathField from './PathField.svelte';

  let s = $state<api.SettingsView | null>(null);
  let modsRoot = $state('');
  api.settings().then((v) => {
    s = v;
    modsRoot = v.mods_root ?? '';
  });

  function save() {
    if (!s) return;
    const { log_dir: _, ...settings } = s;
    run('Saving app settings', () => api.saveSettings({ ...settings, mods_root: modsRoot.trim() || null })).catch(() => {});
  }
</script>

{#if s}
  <div class="setup">
    <strong>App settings</strong>
    <label for="mr">Mods folder for newly set-up games (each game gets a subfolder)</label>
    <PathField id="mr" bind:value={modsRoot} placeholder="Default (app data folder)" />
    <label for="lm">Newly set-up games place mod files by</label>
    <select id="lm" bind:value={s.link_mode}>
      {#each LINK_MODES as [v, t]}<option value={v}>{t}</option>{/each}
    </select>
    <label for="na">Nexus API</label>
    <select id="na" bind:value={s.nexus_api}>
      <option value="graphql">GraphQL (v2)</option>
      <option value="rest">REST (v1)</option>
    </select>
    <p><label class="sub"><input type="checkbox" bind:checked={s.close_on_launch} /> Close Nexus Mod Manager after launching a game</label></p>
    <p><button class="btn primary" onclick={save}>Save</button></p>
    {#if s.log_dir}<p class="muted">Logs: {s.log_dir}</p>{/if}
  </div>
{/if}
