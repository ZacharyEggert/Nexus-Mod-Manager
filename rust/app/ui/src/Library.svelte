<script lang="ts">
  // Installed mods for one game. Every change goes through `op`, which runs it as a task and reloads.
  import * as api from './lib/api';
  import { LINK_MODES } from './lib/ui.svelte';

  let { lib, op, repair }: { lib: api.Library; op: (label: string, o: api.Op) => void; repair: (adopt?: boolean) => void } = $props();

  const order = $derived(Object.fromEntries(lib.mods.map((m, i) => [m.id, i])));
  const name = (id: string) => lib.mods[order[id]]?.name ?? id;

  /** "overrides X; overridden by Y" for mod `i`. */
  function conflictText(m: api.ModEntry, i: number) {
    const others = lib.conflicts[m.id] ?? [];
    const wins = others.filter((o) => order[o] < i).map(name);
    const loses = others.filter((o) => order[o] > i).map(name);
    return [wins.length && `overrides ${wins.join(', ')}`, loses.length && `overridden by ${loses.join(', ')}`].filter(Boolean).join('; ');
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
</p>
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
<div class="drop">Drop .zip / .7z / .rar archives here to install</div>
