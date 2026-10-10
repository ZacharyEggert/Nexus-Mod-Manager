<script lang="ts">
  import * as api from './lib/api';
  import { LINK_MODES, run, runTask } from './lib/ui.svelte';
  import PathField from './PathField.svelte';

  let s = $state<api.SettingsView | null>(null);
  let modsRoot = $state('');
  api.settings().then((v) => {
    s = v;
    modsRoot = v.mods_root ?? '';
  });

  let account = $state<api.Account | null>(null);
  let key = $state('');
  const loadAccount = () => api.nexusAccount().then((a) => (account = a));
  loadAccount();

  const who = (u: api.NexusUser) => `${u.name}${u.premium ? ' (Premium)' : ''}`;

  async function login() {
    const u = await runTask('Logging in to Nexus (approve in your browser)', api.nexusLogin()).catch(() => null);
    if (u) loadAccount();
  }

  async function saveKey() {
    const u = await run('Checking API key', () => api.nexusSetKey(key)).catch(() => null);
    if (u) {
      key = '';
      loadAccount();
    }
  }

  async function logout() {
    await run('Logging out of Nexus', api.nexusLogout).catch(() => {});
    loadAccount();
  }

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
    <label for="md">Downloads at once</label>
    <input id="md" type="number" min="1" max="10" bind:value={s.max_downloads} />
    <p><label class="sub"><input type="checkbox" bind:checked={s.close_on_launch} /> Close Nexus Mod Manager after launching a game</label></p>
    <p><button class="btn primary" onclick={save}>Save</button></p>
    <strong>Nexus account</strong>
    {#if account?.user}
      <p>Logged in as {who(account.user)} <button class="btn" onclick={logout}>Log out</button></p>
    {:else if account}
      {#if account.error}<p class="err">Stored key not usable: {account.error} <button class="btn" onclick={logout}>Forget it</button></p>{/if}
      {#if account.sso}<p><button class="btn primary" onclick={login}>Log in with Nexus Mods</button></p>{/if}
      <label for="ak">Personal API key (from nexusmods.com → Site preferences → API keys)</label>
      <p>
        <input id="ak" type="password" autocomplete="off" bind:value={key} />
        <button class="btn" disabled={!key.trim()} onclick={saveKey}>Use key</button>
      </p>
    {/if}
    {#if s.log_dir}<p class="muted">Logs: {s.log_dir}</p>{/if}
  </div>
{/if}
