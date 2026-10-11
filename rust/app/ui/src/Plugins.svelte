<script lang="ts">
  // Plugin load order for one game: drag or ▲▼ to reorder, tick to activate. Every change is
  // sent whole; the backend re-applies the game's rules and refuses changes that break the order.
  // Shift/Ctrl-click (or Shift+arrows) select several; moves and Space/Enable/Disable apply to all.
  import * as api from './lib/api';
  import { dropSelected, range, stepSelected } from './lib/order';
  import { runTask, status } from './lib/ui.svelte';

  let { mode, loot }: { mode: string; loot: boolean } = $props();

  let rows = $state<api.PluginRow[]>([]);
  let filter = $state('');
  let dragging = $state<string | null>(null);
  /** Last clicked row: details pane, range anchor, keyboard focus. */
  let selected = $state<string | null>(null);
  let picked = $state(new Set<string>());
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

  async function importUserlist() {
    try {
      if (await api.lootImportUserlist(mode)) status('LOOT userlist imported; used from the next sort');
    } catch (e) {
      status(`Not imported: ${e}`, true);
    }
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

  /** Index column: the game's hex slot, or (as C#'s Index column) the 1-based count of indexed plugins. */
  let hex = $state(true);
  try { hex = localStorage.getItem('pluginIndexHex') !== '0'; } catch {}
  function toggleHex() {
    hex = !hex;
    try { localStorage.setItem('pluginIndexHex', hex ? '1' : '0'); } catch {}
  }
  const decimal = $derived.by(() => {
    let n = 0;
    return new Map(rows.filter((r) => r.index != null).map((r) => [r.name, ++n]));
  });
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

  const byName = (names: string[]) => names.map((n) => rows.find((r) => r.name === n)!);
  /** The selection when `name` is part of it, else just `name`. */
  const group = (name: string) => (picked.has(name) ? picked : new Set([name]));
  /** A row the game pins in the group, which blocks moving it (C# refuses the whole move). */
  const pinned = (g: Set<string>) => rows.find((r) => g.has(r.name) && r.locked_order);

  /** Move `name` (with the selection it's in) to just before `before` (null = end). */
  function moveTo(name: string, before: string | null) {
    const g = group(name);
    const p = pinned(g);
    if (p) return status(`${p.name} has a fixed position`, true);
    save(byName(dropSelected(rows.map((r) => r.name), g, before)));
  }

  /** One step among the visible rows, for `name` and the selection it's in. */
  function step(name: string, delta: -1 | 1) {
    const g = group(name);
    const p = pinned(g);
    if (p) return status(`${p.name} has a fixed position`, true);
    const next = stepSelected(rows.map((r) => r.name), shown.map((r) => r.name), g, delta);
    if (next) save(byName(next));
  }

  function setPicked(active: boolean) {
    save(rows.map((r) => (picked.has(r.name) ? { ...r, active: active || r.locked_active } : r)));
  }

  function pick(name: string, e: { shiftKey: boolean; ctrlKey: boolean; metaKey: boolean }) {
    if (e.shiftKey) picked = new Set(range(shown.map((r) => r.name), anchor, name));
    else if (e.ctrlKey || e.metaKey) {
      const next = new Set(picked);
      if (!next.delete(name)) next.add(name);
      picked = next;
      anchor = name;
    } else {
      picked = new Set([name]);
      anchor = name;
    }
    selected = name;
  }

  /** Arrows move focus (Shift extends), Alt+arrows move the selection, Space toggles it, Ctrl+A picks all. */
  function onKey(e: KeyboardEvent, r: api.PluginRow) {
    if ((e.target as HTMLElement).closest('input, button')) return;
    const i = shown.indexOf(r);
    if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
      e.preventDefault();
      const delta = e.key === 'ArrowUp' ? -1 : 1;
      if (e.altKey) return step(r.name, delta);
      const to = shown[i + delta];
      if (!to) return;
      if (e.shiftKey) {
        anchor ??= r.name;
        picked = new Set(range(shown.map((x) => x.name), anchor, to.name));
      } else {
        picked = new Set([to.name]);
        anchor = to.name;
      }
      selected = to.name;
      document.querySelector<HTMLElement>(`tr[data-name="${CSS.escape(to.name)}"]`)?.focus();
    } else if (e.key === ' ') {
      e.preventDefault();
      const g = group(r.name);
      picked = g;
      setPicked(rows.some((x) => g.has(x.name) && !x.active));
    } else if (e.key === 'a' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      picked = new Set(shown.map((x) => x.name));
    }
  }
  /** Shift+arrow range start. */
  let anchor = $state<string | null>(null);

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
  {#if loot}<button class="btn" onclick={sortLoot}>Sort with LOOT</button>
    <button class="btn" onclick={importUserlist}>LOOT userlist…</button>{/if}
  <button class="btn" onclick={exportOrder}>Export…</button>
  <button class="btn" onclick={importOrder}>Import…</button>
</p>
{#if picked.size > 1}
  <p class="muted" aria-live="polite">
    {picked.size} selected:
    <button class="btn" onclick={() => setPicked(true)}>Enable</button>
    <button class="btn" onclick={() => setPicked(false)}>Disable</button>
    <button class="btn" aria-label="Move selected up" onclick={() => step([...picked][0], -1)}>▲</button>
    <button class="btn" aria-label="Move selected down" onclick={() => step([...picked][0], 1)}>▼</button>
    <button class="btn" onclick={() => (picked = new Set())}>Clear</button>
  </p>
{/if}

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
    <table role="grid" aria-multiselectable="true" aria-label="Plugins">
      <thead><tr><th><button class="link" onclick={toggleHex} title="Switch between hex load-order slot and decimal count">{hex ? 'Index (hex)' : 'Index (#)'}</button></th><th>On</th><th>Plugin</th><th>Type</th><th></th></tr></thead>
      <tbody>
        {#each shown as r, i (r.name)}
          {@const errs = r.diagnostics.filter((d) => d.error)}
          {@const warns = r.diagnostics.filter((d) => !d.error)}
          <tr draggable={!r.locked_order} class:sel={picked.has(r.name)} class:drag={dragging === r.name}
            data-name={r.name} tabindex={selected === r.name || (!selected && i === 0) ? 0 : -1} aria-selected={picked.has(r.name)}
            onclick={(e) => { if (!(e.target as HTMLElement).closest('input, button')) pick(r.name, e); }}
            onkeydown={(e) => onKey(e, r)}
            ondragstart={(e) => { dragging = r.name; e.dataTransfer?.setData('text/plain', r.name); }}
            ondragend={() => (dragging = null)}
            ondragover={(e) => dragging && e.preventDefault()}
            ondrop={(e) => { e.preventDefault(); if (dragging && dragging !== r.name) moveTo(dragging, r.name); dragging = null; }}>
            <td class="num">{hex ? (r.index ?? '') : (decimal.get(r.name) ?? '')}</td>
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
                <button title="Load earlier" aria-label="Move {r.name} up" disabled={i === 0} onclick={() => step(r.name, -1)}>▲</button>
                <button title="Load later" aria-label="Move {r.name} down" disabled={i === shown.length - 1} onclick={() => step(r.name, 1)}>▼</button>
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
