<script lang="ts">
  import { getCurrentWebview } from '@tauri-apps/api/webview';
  import * as api from './lib/api';
  import { cancelTask, run, runTask, status, ui } from './lib/ui.svelte';
  import AppSettings from './AppSettings.svelte';
  import Downloads from './Downloads.svelte';
  import Library from './Library.svelte';
  import Plugins from './Plugins.svelte';
  import Setup from './Setup.svelte';
  import Wizard from './Wizard.svelte';

  type View = { kind: 'none' } | { kind: 'settings' } | { kind: 'downloads' } | { kind: 'game'; mode: string; setup: boolean };

  let games = $state<api.GameInfo[]>([]);
  let view = $state<View>({ kind: 'none' });
  const curMode = $derived(view.kind === 'game' ? view.mode : null);
  const cur = $derived(games.find((g) => g.mode_id === curMode));
  /** Library of `cur`, loaded when its view shows. */
  let lib = $state<api.Library | null>(null);
  let wizard = $state<{ token: string; cfg: api.ModuleConfig; finish: (sel: api.Selection | null) => void } | null>(null);
  let profile = $state('');
  let profileName = $state('');
  let launchAnyway = false;
  let tab = $state<'mods' | 'plugins'>('mods');

  async function loadGames() {
    games = await api.games();
  }

  async function select(mode: string) {
    await loadGames();
    const g = games.find((x) => x.mode_id === mode);
    view = { kind: 'game', mode, setup: !g?.game_path };
    lib = null;
    if (g?.game_path) await reload();
  }

  async function reload() {
    if (view.kind !== 'game') return;
    const mode = view.mode;
    launchAnyway = false;
    try {
      const l = await api.library(mode);
      if (view.kind === 'game' && view.mode === mode) lib = l;
    } catch (e) {
      status(String(e), true);
      view = { kind: 'game', mode, setup: true };
    }
  }

  async function op(label: string, o: api.Op) {
    if (view.kind !== 'game') return;
    await runTask(label, api.apply(view.mode, o)).catch(() => {});
    reload();
  }

  const count = (n: number, what: string) => `${n} ${what}${n === 1 ? '' : 's'}`;

  /** Rebuild deploy records from the game folder (`adopt`: re-adding stray mods first); reports what it left for review. */
  async function repair(adopt = false) {
    if (view.kind !== 'game') return;
    const r = await runTask(adopt ? 'Re-adding mods' : 'Scanning game and repairing', (adopt ? api.adopt : api.repair)(view.mode)).catch(() => null);
    const notes = [
      r?.orphaned_backups.length && `${count(r.orphaned_backups.length, 'outdated backup')} moved to OrphanedBackup in the mods folder`,
      r?.untracked.length && `${count(r.untracked.length, 'file')} matching a mod but not recorded, left in place: ${r.untracked.join(', ')}`,
    ].filter(Boolean);
    if (notes.length) status(`Done. ${notes.join('. ')}.`);
    reload();
  }

  async function install(path: string, download: string | null = null) {
    if (view.kind !== 'game' || !cur?.game_path) return status('Pick and set up a game first', true);
    const mode = view.mode;
    const res = await runTask(`Installing ${path.split(/[\\/]/).pop()}`, api.stage(mode, path, download)).catch(() => null);
    if (res?.token && res.fomod) {
      const { token, fomod: cfg } = res;
      const sel = await new Promise<api.Selection | null>((finish) => (wizard = { token, cfg, finish }));
      wizard = null;
      if (sel) await runTask(`Installing ${cfg.name || 'mod'}`, api.fomodInstall(token, sel)).catch(() => {});
      else api.fomodCancel(token);
    }
    if (view.kind === 'game' && view.mode === mode) reload();
  }

  async function pickAndInstall() {
    const p = await api.pickArchive();
    if (p) install(p);
  }

  function saveProfile() {
    const name = profileName.trim();
    if (!name) return status('Type a profile name first', true);
    profileName = '';
    op(`Saving profile ${name}`, { op: 'save_profile', name });
  }

  function launch() {
    if (!cur) return;
    // Out of sync = a deploy failed partway; ask once (second click launches anyway).
    if (lib && !lib.in_sync && !launchAnyway) {
      launchAnyway = true;
      return status('Some mod files aren’t deployed. Redeploy first, or click Launch again to launch anyway.', true);
    }
    launchAnyway = false;
    run(`Launching ${cur.name}`, () => api.launch(cur.mode_id)).catch(() => {});
  }

  /** Route Nexus links to a configured game they belong to (the open one if it matches). */
  async function handleLinks() {
    for (const { link, modes } of await api.takeLinks()) {
      await loadGames();
      const ready = modes.filter((m) => games.find((g) => g.mode_id === m)?.game_path);
      const mode = ready.find((m) => m === curMode) ?? ready[0];
      if (!mode) {
        const names = modes.map((m) => games.find((g) => g.mode_id === m)?.name ?? m);
        status(names.length ? `Set up ${names.join(' or ')} to use this Nexus link` : `No supported game for Nexus "${link.domain}"`, true);
        continue;
      }
      onNexusLink(link, mode);
    }
  }

  async function onNexusLink(link: api.NexusLink, mode: string) {
    view = { kind: 'downloads' };
    lib = null;
    await run(`Adding download (mod ${link.mod_id})`, () => api.downloadAdd(mode, link.source)).catch(() => {});
  }

  /** Install a finished download into its game. */
  async function installDownload(d: api.Download) {
    await select(d.mode);
    if (view.kind === 'game' && !view.setup) install(d.path, d.id);
  }

  async function checkUpdates() {
    if (view.kind !== 'game') return;
    const r = await runTask('Checking Nexus for updates', api.nexusCheck(view.mode)).catch(() => null);
    if (r) {
      const parts = [
        r.identified && `${count(r.identified, 'mod')} matched on Nexus`,
        r.updates.length ? `updates for ${r.updates.join(', ')}` : 'no updates',
        r.errors.length && `${count(r.errors.length, 'problem')}: ${r.errors.join('; ')}`,
      ].filter(Boolean);
      status(parts.join('. ') + '.', r.errors.length > 0);
    }
    reload();
  }

  loadGames().then(handleLinks);
  api.onLibraryChanged((mode) => mode === curMode && reload());
  api.onLinks(handleLinks);
  getCurrentWebview().onDragDropEvent(async (e) => {
    if (e.payload.type !== 'drop') return;
    for (const p of e.payload.paths) await install(p);
  });
</script>

<aside>
  <h1>Nexus Mod Manager</h1>
  <nav>
    {#each games as g (g.mode_id)}
      <button class:sel={cur?.mode_id === g.mode_id} onclick={() => select(g.mode_id)}>
        {g.name}<small>{g.game_path ? 'configured' : g.detected ? 'detected' : 'not set up'}</small>
      </button>
    {/each}
  </nav>
  <div class="foot">
  <button class:sel={view.kind === 'downloads'} onclick={() => { view = { kind: 'downloads' }; lib = null; }}>Downloads</button>
  <button class:sel={view.kind === 'settings'} onclick={() => { view = { kind: 'settings' }; lib = null; }}>App settings</button>
  </div>
</aside>

<main>
  <div class="bar">
    <strong>{view.kind === 'settings' ? 'App settings' : view.kind === 'downloads' ? 'Downloads' : (cur?.name ?? 'Select a game')}</strong>
    <span class="grow"></span>
    {#if view.kind === 'game' && cur?.game_path}
      <button class="btn primary" onclick={pickAndInstall}>Install mod…</button>
      <select aria-label="Profile" bind:value={profile}>
        <option value="">{lib?.profiles.length ? 'Profiles' : 'No profiles'}</option>
        {#each lib?.profiles ?? [] as p}<option value={p}>{p}</option>{/each}
      </select>
      <button class="btn" onclick={() => profile && op(`Loading profile ${profile}`, { op: 'load_profile', name: profile })}>Load</button>
      <input type="text" placeholder="New profile name" aria-label="New profile name" size="14" bind:value={profileName} />
      <button class="btn" onclick={saveProfile}>Save</button>
      <button class="btn" onclick={() => { if (profile) op(`Deleting profile ${profile}`, { op: 'delete_profile', name: profile }); profile = ''; }}>Delete</button>
      <button class="btn" onclick={() => view.kind === 'game' && (view = { ...view, setup: true })}>Settings</button>
      <button class="btn" onclick={checkUpdates}>Check for updates</button>
      <button class="btn" onclick={launch}>Launch</button>
    {/if}
  </div>

  <div id="content">
    {#if view.kind === 'settings'}
      <AppSettings />
    {:else if view.kind === 'downloads'}
      <Downloads {games} oninstall={installDownload} />
    {:else if view.kind === 'game' && cur}
      {#if view.setup}
        {#key cur.mode_id}<Setup game={cur} onsaved={() => select(cur.mode_id)} />{/key}
      {:else if lib}
        {#if lib.uses_plugins}
          <div class="tabs" role="tablist">
            <button role="tab" aria-selected={tab === 'mods'} onclick={() => (tab = 'mods')}>Mods</button>
            <button role="tab" aria-selected={tab === 'plugins'} onclick={() => (tab = 'plugins')}>Plugins</button>
          </div>
        {/if}
        {#if lib.uses_plugins && tab === 'plugins'}
          <Plugins mode={view.mode} loot={lib.loot} />
        {:else}
          <Library {lib} {op} {repair} />
        {/if}
      {/if}
    {/if}
  </div>

  <div id="tasks">
    {#each ui.tasks as t (t.id)}
      <div>
        <span>{t.label}</span>
        <progress aria-label={t.label} max={t.progress?.total || undefined} value={t.progress?.total ? t.progress.done : undefined}></progress>
        <span class="muted">{t.progress ? `${t.progress.step} ${t.progress.done}${t.progress.total ? '/' + t.progress.total : ''}` : ''}</span>
        <button class="btn" disabled={t.cancelling} onclick={() => cancelTask(t)}>Cancel</button>
      </div>
    {/each}
  </div>
  <div id="status" class:err={ui.status.err} role="status" aria-live="polite">{ui.status.msg}</div>
</main>

{#if wizard}
  <Wizard token={wizard.token} cfg={wizard.cfg} onfinish={wizard.finish} />
{/if}
