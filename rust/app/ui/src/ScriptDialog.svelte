<script lang="ts">
  // Runs a staged Mod Script: shows each question it asks, then runs it again with the answers so
  // far. Calls `onfinish(true)` once it ran to the end, false when cancelled or it failed.
  import { onMount } from 'svelte';
  import * as api from './lib/api';
  import { status } from './lib/ui.svelte';

  let { token, name, onfinish }: { token: string; name: string; onfinish: (ok: boolean) => void } = $props();

  let dlg: HTMLDialogElement;
  const answers: api.ScriptAnswer[] = [];
  let q = $state<api.ScriptPrompt | null>(null);
  let picked = $state<number[]>([]);
  let text = $state('');
  let shown = $state<api.ScriptOption | null>(null);
  let images = $state<Record<string, string>>({});
  let busy = $state(false);

  async function load(path: string | null) {
    if (path && !(path in images)) {
      try {
        images[path] = URL.createObjectURL(new Blob([await api.fomodImage(token, path)]));
      } catch {
        images[path] = ''; // image is optional
      }
    }
  }

  async function next(a?: api.ScriptAnswer) {
    if (a !== undefined) answers.push(a);
    busy = true;
    try {
      const p = await api.scriptStep(token, answers);
      if (!p) return finish(true);
      shown = null;
      if (p.kind === 'select') picked = p.options.flatMap((o, i) => (o.default ? [i] : []));
      if (p.kind === 'input') text = p.initial ?? '';
      if (p.kind === 'image') await load(p.path);
      q = p;
      if (!dlg.open) dlg.showModal();
    } catch (e) {
      status(`${name}: ${e}`, true);
      finish(false);
    } finally {
      busy = false;
    }
  }

  function finish(ok: boolean) {
    if (dlg.open) dlg.close();
    Object.values(images).forEach((u) => u && URL.revokeObjectURL(u));
    onfinish(ok);
  }

  function toggle(i: number, many: boolean) {
    if (!many) picked = [i];
    else picked = picked.includes(i) ? picked.filter((x) => x !== i) : [...picked, i].sort((a, b) => a - b);
  }

  function show(o: api.ScriptOption) {
    shown = o;
    load(o.image);
  }

  onMount(() => next());
</script>

<dialog bind:this={dlg} oncancel={(e) => { e.preventDefault(); finish(false); }}>
  {#if q?.kind === 'select'}
    {@const s = q}
    <div class="wiz">
      <header>{s.title || name}</header>
      <div class="opts">
        <fieldset>
          <legend>{s.many ? 'Choose any' : 'Choose one'}</legend>
          {#each s.options as o, i}
            <label onmouseenter={() => show(o)} onfocusin={() => show(o)}>
              <input type={s.many ? 'checkbox' : 'radio'} name="opt" checked={picked.includes(i)} onclick={() => toggle(i, s.many)} />
              {o.name}
            </label>
          {/each}
        </fieldset>
      </div>
      <div class="info">
        {#if shown}
          {#if shown.image && images[shown.image]}<img src={images[shown.image]} alt="" />{/if}
          <div>{shown.description || shown.name}</div>
        {:else}
          <div>Hover an option for details.</div>
        {/if}
      </div>
      <footer>
        <button class="btn" onclick={() => finish(false)}>Cancel</button>
        <button class="btn primary" disabled={busy || (!s.many && picked.length !== 1)} onclick={() => next(picked)}>Next</button>
      </footer>
    </div>
  {:else if q}
    <div class="readme">
      <header>{q.title || name}</header>
      {#if q.kind === 'image'}
        {#if images[q.path]}<img src={images[q.path]} alt={q.title || q.path} />{:else}<pre>Couldn’t show {q.path}.</pre>{/if}
      {:else if q.kind === 'text'}
        <pre>{q.text}</pre>
      {:else if q.kind === 'input'}
        <div style="padding:12px 16px">
          <!-- svelte-ignore a11y_autofocus -->
          <input type="text" style="width:100%" aria-label={q.title || 'Text'} bind:value={text} autofocus onkeydown={(e) => e.key === 'Enter' && next(text)} />
        </div>
      {:else}
        <pre>{q.message}</pre>
      {/if}
      <footer>
        <button class="btn" onclick={() => finish(false)}>Cancel install</button>
        {#if q.kind === 'yes_no'}
          <button class="btn" disabled={busy} onclick={() => next(false)}>No</button>
          <button class="btn primary" disabled={busy} onclick={() => next(true)}>Yes</button>
        {:else if q.kind === 'input'}
          <button class="btn primary" disabled={busy} onclick={() => next(text)}>OK</button>
        {:else}
          <button class="btn primary" disabled={busy} onclick={() => next(null)}>OK</button>
        {/if}
      </footer>
    </div>
  {/if}
</dialog>
