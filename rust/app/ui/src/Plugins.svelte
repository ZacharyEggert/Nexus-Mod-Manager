<script lang="ts">
  // Plugin load order for one game: drag or ▲▼ to reorder, tick to activate. Every change is
  // sent whole; the backend re-applies the game's rules and refuses changes that break the order.
  import * as api from './lib/api';
  import { runTask, status } from './lib/ui.svelte';

  let { mode, loot }: { mode: string; loot: boolean } = $props();

  let rows = $state<api.PluginRow[]>([]);
  let filter = $state('');
  let dragging = $state<string | null>(null);
  let selected = $state<string | null>(null);
  let preview = $state<api.LootPreview | null>(null);
  /** Plugins LOOT moves: new position, old position. */
  const moves = $derived(preview ? preview.sorted.map((n, i) => ({ n, i, was: preview!.current.indexOf(n) })).filter((m) => m.i !== m.was) : []);

  async function sortLoot() {
    preview = await runTask('Sorting with LOOT', api.lootPreview(mode)).catch(() => null);
    if (preview?.note) status(preview.note, true);
  }

  async function applyLoot() {
    const active = new Map(rows.map((r) => [r.name, r.active]));
    if (preview && (await save(preview.sorted.map((name) => ({ name, active: active.get(name) ?? false }))))) status('LOOT order applied');
    preview = null;
  }

  async function exportOrder() {
    try {
      if (await api.pluginsExport(mode)) status('Load order exported');
    } catch (e) {
      status(`Export failed: ${e}`, true);
    }
  }

  async function importOrder() {
    try {
      const r = await api.pluginsImport(mode);
      if (!r) return;
      await load();
      status(
        `Imported ${r.imported} of ${r.total} plugins${r.unknown.length ? `; not in the game: ${r.unknown.join(', ')}` : ''}`,
        r.unknown.length > 0,
      );
    } catch (e) {
      status(`Not imported: ${e}`, true);
    }
  }

  const shown = $derived(rows.filter((r) => !filter || r.name.toLowerCase().includes(filter.toLowerCase()) || r.owner?.toLowerCase().includes(filter.toLowerCase())));
  const used = (c: api.PluginRow['class']) => rows.filter((r) => r.active && r.class === c).length;
  const sel = $derived(rows.find((r) => r.name === selected));

  async function load() {
    try {
      rows = await api.plugins(mode);
    } catch (e) {
      status(String(e), true);
    }
  }

  /** Send the whole list; false when refused (the reasons go to the status line). */
  async function save(next: api.Plugin[]) {
    try {
      rows = await api.pluginsSet(mode, next.map(({ name, active }) => ({ name, active })));
      return true;
    } catch (e) {
      status(`Not changed: ${e}`, true);
      return false;
    }
  }

  async function toggle(r: api.PluginRow, box: HTMLInputElement) {
    const ok = await save(rows.map((x) => (x.name === r.name ? { ...x, active: box.checked } : x)));
    // Refused: state is unchanged, so Svelte won't reset the box itself.
    if (!ok) box.checked = r.active;
  }

  /** Move `name` to just before `before` (null = end). */
  function moveTo(name: string, before: string | null) {
    const r = rows.find((x) => x.name === name)!;
    const rest = rows.filter((x) => x.name !== name);
    const i = before === null ? rest.length : rest.findIndex((x) => x.name === before);
    save([...rest.slice(0, i), r, ...rest.slice(i)]);
  }

  /** One step among the visible rows. */
  function step(r: api.PluginRow, delta: -1 | 1) {
    const i = shown.indexOf(r);
    const other = shown[i + delta];
    if (!other) return;
    moveTo(r.name, delta < 0 ? other.name : (rows[rows.indexOf(other) + 1]?.name ?? null));
  }

  function setAll(active: boolean) {
    save(rows.map((r) => ({ ...r, active: active || r.locked_active })));
  }

  const flags = (r: api.PluginRow) =>
    [r.header.master && 'ESM', r.header.light && 'ESL', r.header.medium && 'Medium', r.header.blueprint && 'Blueprint', r.header.unreadable && 'unreadable'].filter(Boolean).join(' · ');

  $effect(() => {
    const m = mode;
    load();
    const off = api.onLibraryChanged((x) => x === m && load());
    return () => void off.then((f) => f());
  });
</script>

<p class="muted">
  {rows.filter((r) => r.active).length} of {rows.length} active · {used('full')} full{used('light') ? ` · ${used('light')} light` : ''}{used('medium') ? ` · ${used('medium')} medium` : ''}.
  Lower in the list loads later and wins.
  <input type="text" placeholder="Filter" aria-label="Filter plugins" size="18" bind:value={filter} />
  <button class="btn" onclick={() => setAll(true)}>Enable all</button>
  <button class="btn" onclick={() => setAll(false)}>Disable all</button>
  {#if loot}<button class="btn" onclick={sortLoot}>Sort with LOOT</button>{/if}
  <button class="btn" onclick={exportOrder}>Export…</button>
  <button class="btn" onclick={importOrder}>Import…</button>
</p>

{#if preview}
  <section class="loot" aria-label="LOOT sort preview">
    {#if moves.length}
      <p>LOOT moves {moves.length} plugin{moves.length === 1 ? '' : 's'}:</p>
      <ol>
        {#each moves as m (m.n)}<li>{m.n} <span class="muted">#{m.was + 1} → #{m.i + 1}</span></li>{/each}
      </ol>
      <button class="btn primary" onclick={applyLoot}>Apply</button>
    {:else}
      <p>Already in LOOT's order.</p>
    {/if}
    <button class="btn" onclick={() => (preview = null)}>{moves.length ? 'Discard' : 'Close'}</button>
  </section>
{/if}

{#if rows.length}
  <div class="plugins">
    <table>
      <thead><tr><th>Index</th><th>On</th><th>Plugin</th><th>Type</th><th></th></tr></thead>
      <tbody>
        {#each shown as r, i (r.name)}
          {@const errs = r.diagnostics.filter((d) => d.error)}
          {@const warns = r.diagnostics.filter((d) => !d.error)}
          <tr draggable={!r.locked_order} class:sel={selected === r.name} class:drag={dragging === r.name}
            onclick={() => (selected = r.name)}
            ondragstart={(e) => { dragging = r.name; e.dataTransfer?.setData('text/plain', r.name); }}
            ondragend={() => (dragging = null)}
            ondragover={(e) => dragging && e.preventDefault()}
            ondrop={(e) => { e.preventDefault(); if (dragging && dragging !== r.name) moveTo(dragging, r.name); dragging = null; }}>
            <td class="num">{r.index ?? ''}</td>
            <td>
              <input type="checkbox" checked={r.active} disabled={r.locked_active} aria-label="Activate {r.name}"
                title={r.locked_active ? 'The game always loads this plugin' : ''}
                onchange={(e) => toggle(r, e.currentTarget)} />
            </td>
            <td>
              {r.name}
              {#if r.owner}<span class="muted"> · {r.owner}</span>{/if}
              {#each errs as d}<div class="err small">{d.message}</div>{/each}
              {#each warns as d}<div class="conf">{d.message}</div>{/each}
            </td>
            <td class="muted small">{flags(r)}</td>
            <td class="act">
              {#if r.locked_order}
                <span class="muted small" title="The game fixes this plugin's position">fixed</span>
              {:else}
                <button title="Load earlier" aria-label="Move {r.name} up" disabled={i === 0} onclick={() => step(r, -1)}>▲</button>
                <button title="Load later" aria-label="Move {r.name} down" disabled={i === shown.length - 1} onclick={() => step(r, 1)}>▼</button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
    {#if sel}
      <section class="details" aria-label="Plugin details">
        <h3>{sel.name}</h3>
        {#if sel.header.description}<p>{sel.header.description}</p>{/if}
        <p class="muted">Masters: {sel.header.masters.length ? sel.header.masters.join(', ') : 'none'}</p>
        {#if sel.owner}<p class="muted">From mod: {sel.owner}</p>{/if}
      </section>
    {/if}
  </div>
{:else}
  <p>No plugins in the game's data folder yet.</p>
{/if}
