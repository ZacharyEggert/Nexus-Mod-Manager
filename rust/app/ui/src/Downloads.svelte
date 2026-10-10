<script lang="ts">
  import * as api from './lib/api';
  import { run, status } from './lib/ui.svelte';

  let { games, oninstall }: { games: api.GameInfo[]; oninstall: (d: api.Download) => void } = $props();

  let list = $state<api.Download[]>([]);
  let url = $state('');
  let mode = $state('');
  api.downloads().then((l) => (list = l));
  const stop = api.onDownload((d) => {
    const i = list.findIndex((x) => x.id === d.id);
    if (i >= 0) list[i] = d;
    else list.push(d);
  });
  $effect(() => () => {
    stop.then((f) => f());
  });

  const configured = $derived(games.filter((g) => g.game_path));
  const gameName = (m: string) => games.find((g) => g.mode_id === m)?.name ?? m;
  const mb = (n: number) => (n / 1048576).toFixed(1);

  async function add() {
    if (!mode) return status('Pick the game this download is for', true);
    const id = await run('Adding download', () => api.downloadAdd(mode, url.trim())).catch(() => null);
    if (id) url = '';
  }

  async function remove(d: api.Download) {
    await api.downloadRemove(d.id, false);
    list = list.filter((x) => x.id !== d.id);
  }
</script>

<div class="setup">
  <label for="durl">Add from Nexus (nxm:// link or mod page)</label>
  <p>
    <input id="durl" type="text" size="50" placeholder="nxm://skyrimspecialedition/mods/12604/files/749043" bind:value={url} />
    <select aria-label="Game" bind:value={mode}>
      <option value="">Game…</option>
      {#each configured as g}<option value={g.mode_id}>{g.name}</option>{/each}
    </select>
    <button class="btn primary" disabled={!url.trim()} onclick={add}>Download</button>
  </p>
</div>

{#if list.length === 0}
  <p class="muted">No downloads yet. Use “Mod Manager Download” on nexusmods.com, or paste a link above.</p>
{:else}
  <table class="downloads">
    <tbody>
      {#each list as d (d.id)}
        <tr>
          <td>
            <strong>{d.mod_name}</strong> — {d.name} {d.version}
            <div class="muted">{gameName(d.mode)}{d.verified === true ? ' · verified' : d.state === 'done' && d.verified === null ? ' · not verified' : ''}</div>
            {#if d.error}<div class:err={d.state === 'failed'} class:muted={d.state !== 'failed'}>{d.error}</div>{/if}
          </td>
          <td>
            {#if d.state === 'done'}
              done
            {:else}
              <progress aria-label="{d.name} progress" max={d.size || undefined} value={d.size ? d.done : undefined}></progress>
              <div class="muted">{d.state} · {mb(d.done)}{d.size ? ` / ${mb(d.size)}` : ''} MB</div>
            {/if}
          </td>
          <td class="actions">
            {#if d.state === 'running' || d.state === 'queued'}
              <button class="btn" onclick={() => api.downloadPause(d.id)}>Pause</button>
            {:else if d.state === 'paused' || d.state === 'failed'}
              <button class="btn" onclick={() => api.downloadResume(d.id)}>{d.state === 'failed' ? 'Retry' : 'Resume'}</button>
            {:else}
              <button class="btn primary" onclick={() => oninstall(d)}>Install</button>
            {/if}
            <button class="btn" onclick={() => remove(d)}>Remove</button>
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}
