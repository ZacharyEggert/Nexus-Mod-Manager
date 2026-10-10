<script lang="ts">
  // FOMod installer wizard. Calls `onfinish` with the selection, or null when cancelled.
  import { onMount } from 'svelte';
  import { SvelteSet } from 'svelte/reactivity';
  import * as api from './lib/api';

  let { token, cfg, onfinish }: { token: string; cfg: api.ModuleConfig; onfinish: (sel: api.Selection | null) => void } = $props();

  let dlg: HTMLDialogElement;
  const sel = new SvelteSet<string>();
  const inited = new Set<number>();
  const key = (s: number, g: number, p: number) => `${s},${g},${p}`;
  const selArr = (): api.Selection => [...sel].map((k) => k.split(',').map(Number) as [number, number, number]);

  let step = $state(0);
  let ev = $state<api.Eval | null>(null);
  let shown = $state<api.FomodPlugin | null>(null);
  let images = $state<Record<string, string>>({});

  const nextVisible = (e: api.Eval, from: number, dir: 1 | -1) => {
    for (let i = from + dir; i >= 0 && i < cfg.steps.length; i += dir) if (e.visible[i]) return i;
    return -1;
  };

  function initDefaults(e: api.Eval, si: number) {
    cfg.steps[si].groups.forEach((g, gi) => {
      const types = e.types[si][gi];
      g.plugins.forEach((_, pi) => {
        if (g.kind === 'SelectAll' || types[pi] === 'Required' || types[pi] === 'Recommended') sel.add(key(si, gi, pi));
      });
      const chosen = g.plugins.filter((_, pi) => sel.has(key(si, gi, pi))).length;
      if (g.kind === 'SelectExactlyOne' && chosen === 0) {
        const first = types.findIndex((t) => t !== 'NotUsable');
        if (first >= 0) sel.add(key(si, gi, first));
      }
    });
    inited.add(si);
  }

  /** Re-evaluate conditions after a selection or step change. */
  async function refresh() {
    let e = await api.fomodEval(token, selArr());
    if (!e.visible[step]) step = Math.max(0, nextVisible(e, step, 1));
    if (cfg.steps.length && !inited.has(step)) {
      initDefaults(e, step);
      e = await api.fomodEval(token, selArr());
    }
    cfg.steps[step]?.groups.forEach((g, gi) =>
      g.plugins.forEach((_, pi) => {
        if (e.types[step][gi][pi] === 'NotUsable') sel.delete(key(step, gi, pi));
      }),
    );
    ev = e;
  }

  function toggle(g: api.FomodGroup, gi: number, pi: number, checked: boolean) {
    const k = key(step, gi, pi);
    if (g.kind === 'SelectExactlyOne' || g.kind === 'SelectAtMostOne') {
      const was = sel.has(k);
      g.plugins.forEach((_, j) => sel.delete(key(step, gi, j)));
      if (!was || g.kind === 'SelectExactlyOne') sel.add(k);
    } else if (checked) sel.add(k);
    else sel.delete(k);
    refresh();
  }

  async function show(p: api.FomodPlugin) {
    shown = p;
    if (p.image && !(p.image in images)) {
      try {
        images[p.image] = URL.createObjectURL(new Blob([await api.fomodImage(token, p.image)]));
      } catch {
        images[p.image] = ''; // image is optional
      }
    }
  }

  const problems = $derived(
    (cfg.steps[step]?.groups ?? []).flatMap((g, gi) => {
      const n = g.plugins.filter((_, pi) => sel.has(key(step, gi, pi))).length;
      if (g.kind === 'SelectExactlyOne' && n !== 1) return [`Choose one option in “${g.name}”.`];
      if (g.kind === 'SelectAtLeastOne' && n < 1) return [`Choose at least one option in “${g.name}”.`];
      return [];
    }),
  );
  const back = $derived(ev ? nextVisible(ev, step, -1) : -1);
  const fwd = $derived(ev ? nextVisible(ev, step, 1) : -1);

  function go(to: number) {
    step = to;
    refresh();
  }

  function finish(s: api.Selection | null) {
    dlg.close();
    onfinish(s);
  }

  onMount(async () => {
    await refresh();
    dlg.showModal();
  });
</script>

<dialog bind:this={dlg} oncancel={(e) => { e.preventDefault(); finish(null); }}>
  {#if ev}
    {@const s = cfg.steps[step]}
    <div class="wiz">
      <header>{cfg.name || 'Installer'}{s ? ' — ' + s.name : ''}</header>
      <div class="opts">
        {#if s}
          {#each s.groups as g, gi}
            {@const exclusive = g.kind === 'SelectExactlyOne' || g.kind === 'SelectAtMostOne'}
            <fieldset>
              <legend>{g.name}</legend>
              {#each g.plugins as p, pi}
                {@const t = ev.types[step][gi][pi]}
                {@const locked = g.kind === 'SelectAll' || t === 'Required' || t === 'NotUsable'}
                <label class:off={t === 'NotUsable'} onmouseenter={() => show(p)} onfocusin={() => show(p)}>
                  <input
                    type={exclusive ? 'radio' : 'checkbox'}
                    name="g{gi}"
                    checked={sel.has(key(step, gi, pi))}
                    disabled={locked}
                    onclick={(e) => toggle(g, gi, pi, e.currentTarget.checked)}
                  />
                  {p.name}{#if t === 'Recommended'}<span class="muted"> (recommended)</span>{/if}
                </label>
              {/each}
            </fieldset>
          {/each}
        {:else}
          <p>Ready to install.</p>
        {/if}
        {#if problems.length}<p class="conf">{problems.join(' ')}</p>{/if}
      </div>
      <div class="info">
        {#if shown}
          {#if shown.image && images[shown.image]}<img src={images[shown.image]} alt="" />{/if}
          <div>{shown.description || shown.name}</div>
        {:else}
          Hover an option for details.
        {/if}
      </div>
      <footer>
        <button class="btn" onclick={() => finish(null)}>Cancel</button>
        <button class="btn" disabled={back < 0} onclick={() => go(back)}>Back</button>
        {#if fwd >= 0}
          <button class="btn primary" disabled={problems.length > 0} onclick={() => go(fwd)}>Next</button>
        {:else}
          <button class="btn primary" disabled={problems.length > 0} onclick={() => finish(selArr())}>Install</button>
        {/if}
      </footer>
    </div>
  {/if}
</dialog>
