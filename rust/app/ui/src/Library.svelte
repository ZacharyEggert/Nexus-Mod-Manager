<script lang="ts">
  // Installed mods for one game. Every change goes through `op`, which runs it as a task and reloads.
  import * as api from './lib/api';
  import { LINK_MODES, run } from './lib/ui.svelte';

  let { mode, lib, op, repair, loose }: { mode: string; lib: api.Library; op: (label: string, o: api.Op) => void; repair: (adopt?: boolean) => void; loose: () => void } = $props();

  const order = $derived(Object.fromEntries(lib.mods.map((m, i) => [m.id, i])));
  const name = (id: string) => lib.mods[order[id]]?.name ?? id;

  /** "overrides X; overridden by Y" for mod `i`. */
  function conflictText(m: api.ModEntry, i: number) {
    const others = lib.conflicts[m.id] ?? [];
    const wins = others.filter((o) => order[o] < i).map(name);
    const loses = others.filter((o) => order[o] > i).map(name);
    return [wins.length && `overrides ${wins.join(', ')}`, loses.length && `overridden by ${loses.join(', ')}`].filter(Boolean).join('; ');
  }

  const base = (path: string) => path.slice(path.lastIndexOf('/') + 1);
  let readme = $state<{ title: string; text?: string; image?: string } | null>(null);
  let dlg: HTMLDialogElement;

  async function showReadme(m: api.ModEntry, path: string) {
    const text = await run(`Opening ${base(path)}`, () => api.readme(mode, m.id, path)).catch(() => null);
    if (text === null) return;
    readme = { title: `${m.name} — ${base(path)}`, text };
    dlg.showModal();
  }

  async function showScreenshot(m: api.ModEntry) {
    const bytes = await run('Opening screenshot', () => api.screenshot(mode, m.id)).catch(() => null);
    if (bytes === null) return;
    readme = { title: `${m.name} — screenshot`, image: URL.createObjectURL(new Blob([bytes])) };
    dlg.showModal();
  }

  const crossDrive = $derived(
    lib.link_mode === 'hardlink'
      ? 'The mods folder is on a different drive than the game, so hardlinks can’t be made. Pick another mode or move the mods folder in Settings.'
      : lib.link_mode === 'symlink'
        ? 'The mods folder is on a different drive than the game. Symlinks work, but break if that drive is disconnected.'
        : 'The mods folder is on a different drive than the game, so mod files are copied (uses extra space). Symlinks avoid the copy; moving the mods folder to the game’s drive avoids both.',
  );
</script>

<p class="muted">
  Deploying to {lib.install_root} by
  <select aria-label="How mod files are placed in the game" value={lib.link_mode}
    onchange={(e) => op('Relinking mod files', { op: 'set_link_mode', mode: e.currentTarget.value as api.LinkMode })}>
    {#each LINK_MODES as [v, t]}<option value={v}>{t}</option>{/each}
  </select>. Lower in the list wins file conflicts.
  <button class="btn" title="Check the game folder against your mods and fix what’s out of step, e.g. after a crash or a deleted deploy.journal"
    onclick={() => repair()}>Scan &amp; repair</button>
  {#if lib.has_base_files}
    <button class="btn" title="Find files in the game's Data folder that aren't from the base game or your mods" onclick={loose}>Loose files…</button>
  {/if}
</p>
{#if lib.archive_invalidation !== null}
  <!-- keyed: a refused change leaves the value as it was, and only a re-render resets the box -->
  {#key lib}<p class="muted">
    <label title="Makes the game use loose mod files (textures, meshes) instead of the same files inside its own BSA archives. Edits the game's INI.">
      <input type="checkbox" checked={lib.archive_invalidation}
        onchange={(e) => op(`${e.currentTarget.checked ? 'Enabling' : 'Disabling'} archive invalidation`, { op: 'set_archive_invalidation', on: e.currentTarget.checked })} />
      Archive invalidation (loose mod files override the game’s BSAs)
    </label>
  </p>{/key}
{/if}
{#if !lib.same_volume && lib.link_mode !== 'copy'}<p class="conf">{crossDrive}</p>{/if}
{#if lib.strays.length}
  <p class="conf">
    Found stored mods missing from the list (the saved list was lost): {lib.strays.join(', ')}.
    Mods already in the game come back enabled; the rest stay off.
    <button class="btn" onclick={() => repair(true)}>Re-add</button>
  </p>
{/if}
{#if !lib.in_sync}
  <p class="conf">
    Some mod files aren’t deployed (an earlier deploy failed partway).
    <button class="btn" onclick={() => op('Redeploying', { op: 'redeploy' })}>Redeploy</button>
  </p>
{/if}

{#if lib.mods.length}
  <table>
    <thead><tr><th>#</th><th>On</th><th>Mod</th><th>Files</th><th></th></tr></thead>
    <tbody>
      {#each lib.mods as m, i (m.id)}
        {@const conf = conflictText(m, i)}
        {@const roots = m.root_files ?? []}
        <tr>
          <td class="num">{i + 1}</td>
          <td>
            <input type="checkbox" checked={m.active} aria-label="Enable {m.name}"
              onchange={(e) => op(`${e.currentTarget.checked ? 'Enabling' : 'Disabling'} ${m.name}`, { op: 'set_active', id: m.id, active: e.currentTarget.checked })} />
          </td>
          <td>
            {m.name}
            {#if m.nexus}
              {@const n = m.nexus}
              <div class="muted">
                {n.version && `v${n.version}`}{n.author && ` · ${n.author}`}{n.category_id && lib.categories[n.category_id] ? ` · ${lib.categories[n.category_id]}` : ''}
                <button class="link" title="Open on nexusmods.com" onclick={() => api.openNexus(n.domain, n.mod_id)}>Nexus</button>
              </div>
              {#if n.update}
                <button class="badge" title="Open the new file on nexusmods.com" onclick={() => api.openNexus(n.domain, n.mod_id, n.update!.file_id)}>
                  Update available: {n.update.name} {n.update.version}
                </button>
              {/if}
            {:else if m.info && (m.info.version || m.info.author)}
              <div class="muted" title={m.info.id && `Nexus mod ${m.info.id}`}>{[m.info.version && `v${m.info.version}`, m.info.author].filter(Boolean).join(' · ')}</div>
            {/if}
            {#if m.screenshot}
              <div class="muted"><button class="link" onclick={() => showScreenshot(m)}>Screenshot</button></div>
            {/if}
            {#if m.readmes?.length}
              <div class="muted">Readme:
                {#each m.readmes as r (r)}<button class="link" title={r} onclick={() => showReadme(m, r)}>{base(r)}</button>{/each}
              </div>
            {/if}
            {#if conf}<div class="conf">{conf}</div>{/if}
          </td>
          <td class="muted">
            {m.files.length + (m.game_root ? roots.length : 0)} files
            {#if roots.length}
              <label class="sub" title={roots.join('\n')}>
                <input type="checkbox" checked={!!m.game_root}
                  onchange={(e) => op(`${e.currentTarget.checked ? 'Deploying' : 'Withdrawing'} ${m.name} game-root files`, { op: 'set_game_root', id: m.id, on: e.currentTarget.checked })} />
                {roots.length} in game root
              </label>
            {/if}
          </td>
          <td class="act">
            <button title="Load earlier" aria-label="Move {m.name} up" disabled={i === 0} onclick={() => op('Reordering', { op: 'reorder', id: m.id, delta: -1 })}>▲</button>
            <button title="Load later (wins conflicts)" aria-label="Move {m.name} down" disabled={i === lib.mods.length - 1}
              onclick={() => op('Reordering', { op: 'reorder', id: m.id, delta: 1 })}>▼</button>
            <button title="Uninstall" aria-label="Uninstall {m.name}" onclick={() => op(`Removing ${m.name}`, { op: 'remove', id: m.id })}>✕</button>
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
{:else}
  <p>No mods installed yet.</p>
{/if}
<dialog bind:this={dlg} onclose={() => { if (readme?.image) URL.revokeObjectURL(readme.image); readme = null; }}>
  {#if readme}
    <div class="readme">
      <header>{readme.title}</header>
      {#if readme.image}<img src={readme.image} alt="{readme.title}" />{:else}<pre>{readme.text}</pre>{/if}
      <footer><button class="btn" onclick={() => dlg.close()}>Close</button></footer>
    </div>
  {/if}
</dialog>
<div class="drop">Drop .zip / .7z / .rar archives here to install</div>
