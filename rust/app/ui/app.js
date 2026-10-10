const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);
const h = (tag, props = {}, ...kids) => {
  const el = Object.assign(document.createElement(tag), props);
  el.append(...kids.flat().filter((k) => k != null));
  return el;
};

let games = [];
let cur = null;

function status(msg, err = false) {
  $('#status').textContent = msg || '';
  $('#status').className = err ? 'err' : '';
}

async function run(label, fn) {
  status(label + '…');
  try {
    const r = await fn();
    status(label + ' — done');
    return r;
  } catch (e) {
    status(`${label} failed: ${e}`, true);
    throw e;
  }
}

// ---------- games ----------

async function loadGames() {
  games = await invoke('games');
  $('#games').replaceChildren(
    ...games.map((g) =>
      h('button', { className: cur?.mode_id === g.mode_id ? 'sel' : '', onclick: () => select(g.mode_id) },
        g.name,
        h('small', { textContent: g.game_path ? 'configured' : g.detected ? 'detected' : 'not set up' })),
    ),
  );
}

async function select(mode) {
  cur = games.find((g) => g.mode_id === mode);
  $('#title').textContent = cur.name;
  await loadGames();
  cur.game_path ? renderLibrary() : renderSetup();
}

function renderSetup() {
  $('#tools').hidden = !cur.game_path;
  const gp = h('input', { type: 'text', id: 'gp', value: cur.game_path || cur.detected || '' });
  const md = h('input', { type: 'text', id: 'md', value: cur.mods_dir || '', placeholder: 'Default (app data folder)' });
  const browse = (input) => h('button', { className: 'btn', textContent: 'Browse…', onclick: async () => {
    const p = await invoke('pick_folder');
    if (p) input.value = p;
  } });
  $('#content').replaceChildren(
    h('div', { className: 'setup' },
      h('strong', { textContent: `Set up ${cur.name}` }),
      h('label', { htmlFor: 'gp', textContent: 'Game folder' }), h('div', { className: 'row' }, gp, browse(gp)),
      h('label', { htmlFor: 'md', textContent: 'Mods folder (same drive as the game, so files can be hard-linked)' }),
      h('div', { className: 'row' }, md, browse(md)),
      h('p', {},
        h('button', { className: 'btn primary', textContent: 'Save', onclick: async () => {
          await run('Saving settings', () => invoke('configure', { mode: cur.mode_id, gamePath: gp.value, modsDir: md.value || null }));
          await loadGames();
          select(cur.mode_id);
        } }))));
}

// ---------- library ----------

const LINK_MODES = [
  ['auto', 'hardlink, or copy if not possible'],
  ['hardlink', 'hardlink only'],
  ['symlink', 'symlink'],
  ['copy', 'copying'],
];

async function op(label, o) {
  await run(label, () => invoke('apply', { mode: cur.mode_id, op: o }));
  renderLibrary();
}

async function renderLibrary() {
  $('#tools').hidden = false;
  let lib;
  try {
    lib = await invoke('library', { mode: cur.mode_id });
  } catch (e) {
    status(String(e), true);
    return renderSetup();
  }
  $('#profiles').replaceChildren(
    h('option', { value: '', textContent: lib.profiles.length ? 'Profiles' : 'No profiles' }),
    ...lib.profiles.map((p) => h('option', { value: p, textContent: p })),
  );
  const order = Object.fromEntries(lib.mods.map((m, i) => [m.id, i]));
  const name = (id) => lib.mods[order[id]]?.name ?? id;
  const rows = lib.mods.map((m, i) => {
    const others = lib.conflicts[m.id] || [];
    const wins = others.filter((o) => order[o] < i).map(name);
    const loses = others.filter((o) => order[o] > i).map(name);
    const conf = [wins.length && `overrides ${wins.join(', ')}`, loses.length && `overridden by ${loses.join(', ')}`].filter(Boolean).join('; ');
    return h('tr', {},
      h('td', { className: 'num', textContent: i + 1 }),
      h('td', {}, h('input', { type: 'checkbox', checked: m.active, ariaLabel: `Enable ${m.name}`,
        onchange: (e) => op(e.target.checked ? `Enabling ${m.name}` : `Disabling ${m.name}`, { op: 'set_active', id: m.id, active: e.target.checked }) })),
      h('td', {}, m.name, conf && h('div', { className: 'conf', textContent: conf })),
      h('td', { className: 'muted' }, `${m.files.length + (m.game_root ? m.root_files.length : 0)} files`,
        m.root_files?.length && h('label', { className: 'sub', title: m.root_files.join('\n') },
          h('input', { type: 'checkbox', checked: !!m.game_root,
            onchange: (e) => op(`${e.target.checked ? 'Deploying' : 'Withdrawing'} ${m.name} game-root files`, { op: 'set_game_root', id: m.id, on: e.target.checked }) }),
          ` ${m.root_files.length} in game root`)),
      h('td', { className: 'act' },
        h('button', { textContent: '▲', title: 'Load earlier', ariaLabel: `Move ${m.name} up`, disabled: i === 0, onclick: () => op('Reordering', { op: 'reorder', id: m.id, delta: -1 }) }),
        h('button', { textContent: '▼', title: 'Load later (wins conflicts)', ariaLabel: `Move ${m.name} down`, disabled: i === lib.mods.length - 1, onclick: () => op('Reordering', { op: 'reorder', id: m.id, delta: 1 }) }),
        h('button', { textContent: '✕', title: 'Uninstall', ariaLabel: `Uninstall ${m.name}`, onclick: () => op(`Removing ${m.name}`, { op: 'remove', id: m.id }) })));
  });
  $('#content').replaceChildren(
    h('p', { className: 'muted' }, `Deploying to ${lib.install_root} by `,
      h('select', { ariaLabel: 'How mod files are placed in the game', onchange: (e) => op('Relinking mod files', { op: 'set_link_mode', mode: e.target.value }) },
        LINK_MODES.map(([v, t]) => h('option', { value: v, textContent: t, selected: v === lib.link_mode }))),
      '. Lower in the list wins file conflicts.'),
    !lib.same_volume && lib.link_mode !== 'copy' && h('p', { className: 'conf', textContent: lib.link_mode === 'hardlink'
      ? 'The mods folder is on a different drive than the game, so hardlinks can’t be made. Pick another mode or move the mods folder in Settings.'
      : lib.link_mode === 'symlink'
        ? 'The mods folder is on a different drive than the game. Symlinks work, but break if that drive is disconnected.'
        : 'The mods folder is on a different drive than the game, so mod files are copied (uses extra space). Symlinks avoid the copy; moving the mods folder to the game’s drive avoids both.' }),
    lib.mods.length
      ? h('table', {}, h('thead', {}, h('tr', {}, h('th', { textContent: '#' }), h('th', { textContent: 'On' }), h('th', { textContent: 'Mod' }), h('th', { textContent: 'Files' }), h('th'))), h('tbody', {}, rows))
      : h('p', { textContent: 'No mods installed yet.' }),
    h('div', { className: 'drop', textContent: 'Drop .zip / .7z / .rar archives here to install' }),
  );
}

async function install(path) {
  if (!cur?.game_path) return status('Pick and set up a game first', true);
  const res = await run(`Installing ${path.split(/[\\/]/).pop()}`, () => invoke('stage', { mode: cur.mode_id, archive: path }));
  if (res.token) await wizard(res.token, res.fomod);
  renderLibrary();
}

// ---------- FOMod wizard ----------

function wizard(token, cfg) {
  return new Promise((resolve) => {
    const dlg = $('#wizard');
    const sel = new Set();
    const inited = new Set();
    const key = (s, g, p) => `${s},${g},${p}`;
    const selArr = () => [...sel].map((k) => k.split(',').map(Number));
    let step = 0;
    let ev;
    const imgCache = {};

    const close = (installed) => {
      dlg.close();
      if (!installed) invoke('fomod_cancel', { token });
      resolve();
    };

    async function showInfo(info, plugin) {
      const kids = [];
      if (plugin.image) {
        try {
          imgCache[plugin.image] ??= URL.createObjectURL(new Blob([await invoke('fomod_image', { token, path: plugin.image })]));
          kids.push(h('img', { src: imgCache[plugin.image], alt: '' }));
        } catch { /* image is optional */ }
      }
      kids.push(h('div', { textContent: plugin.description || plugin.name }));
      info.replaceChildren(...kids);
    }

    function initDefaults(si) {
      cfg.steps[si].groups.forEach((g, gi) => {
        const types = ev.types[si][gi];
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

    function problems(si) {
      return cfg.steps[si].groups.flatMap((g, gi) => {
        const n = g.plugins.filter((_, pi) => sel.has(key(si, gi, pi))).length;
        if (g.kind === 'SelectExactlyOne' && n !== 1) return [`Choose one option in “${g.name}”.`];
        if (g.kind === 'SelectAtLeastOne' && n < 1) return [`Choose at least one option in “${g.name}”.`];
        return [];
      });
    }

    const nextVisible = (from, dir) => {
      for (let i = from + dir; i >= 0 && i < cfg.steps.length; i += dir) if (ev.visible[i]) return i;
      return -1;
    };

    async function render() {
      ev = await invoke('fomod_eval', { token, sel: selArr() });
      if (!ev.visible[step]) step = Math.max(0, nextVisible(step, 1));
      if (cfg.steps.length && !inited.has(step)) {
        initDefaults(step);
        ev = await invoke('fomod_eval', { token, sel: selArr() });
      }
      const s = cfg.steps[step];
      const info = h('div', { className: 'info', textContent: 'Hover an option for details.' });
      const groups = (s?.groups || []).map((g, gi) => {
        const exclusive = g.kind === 'SelectExactlyOne' || g.kind === 'SelectAtMostOne';
        return h('fieldset', {}, h('legend', { textContent: g.name }),
          g.plugins.map((p, pi) => {
            const t = ev.types[step][gi][pi];
            const locked = g.kind === 'SelectAll' || t === 'Required' || t === 'NotUsable';
            if (t === 'NotUsable') sel.delete(key(step, gi, pi));
            const input = h('input', {
              type: exclusive ? 'radio' : 'checkbox', name: `g${gi}`, checked: sel.has(key(step, gi, pi)), disabled: locked,
              onclick: (e) => {
                const k = key(step, gi, pi);
                if (exclusive) {
                  const was = sel.has(k);
                  g.plugins.forEach((_, j) => sel.delete(key(step, gi, j)));
                  if (!was || g.kind === 'SelectExactlyOne') sel.add(k);
                } else if (e.target.checked) sel.add(k);
                else sel.delete(k);
                render();
              },
            });
            return h('label', { className: locked && t === 'NotUsable' ? 'off' : '', onmouseenter: () => showInfo(info, p), onfocusin: () => showInfo(info, p) },
              input, ' ', p.name, t === 'Recommended' ? h('span', { className: 'muted', textContent: ' (recommended)' }) : null);
          }));
      });
      const back = nextVisible(step, -1);
      const fwd = nextVisible(step, 1);
      const err = s ? problems(step) : [];
      dlg.replaceChildren(
        h('div', { className: 'wiz' },
          h('header', { textContent: `${cfg.name || 'Installer'}${s ? ' — ' + s.name : ''}` }),
          h('div', { className: 'opts' }, groups.length ? groups : h('p', { textContent: 'Ready to install.' }), err.length ? h('p', { className: 'conf', textContent: err.join(' ') }) : null),
          info,
          h('footer', {},
            h('button', { className: 'btn', textContent: 'Cancel', onclick: () => close(false) }),
            h('button', { className: 'btn', textContent: 'Back', disabled: back < 0, onclick: () => { step = back; render(); } }),
            fwd >= 0
              ? h('button', { className: 'btn primary', textContent: 'Next', disabled: err.length > 0, onclick: () => { step = fwd; render(); } })
              : h('button', { className: 'btn primary', textContent: 'Install', disabled: err.length > 0, onclick: async () => {
                  await run('Installing', () => invoke('fomod_install', { token, sel: selArr() }));
                  close(true);
                } }))));
    }

    dlg.oncancel = (e) => { e.preventDefault(); close(false); };
    render().then(() => dlg.showModal());
  });
}

// ---------- toolbar ----------

$('#install').onclick = async () => {
  const p = await invoke('pick_archive');
  if (p) install(p);
};
$('#pload').onclick = () => $('#profiles').value && op(`Loading profile ${$('#profiles').value}`, { op: 'load_profile', name: $('#profiles').value });
$('#pdel').onclick = () => $('#profiles').value && op(`Deleting profile ${$('#profiles').value}`, { op: 'delete_profile', name: $('#profiles').value });
$('#psave').onclick = () => {
  const name = $('#pname').value.trim();
  if (!name) return status('Type a profile name first', true);
  $('#pname').value = '';
  op(`Saving profile ${name}`, { op: 'save_profile', name });
};
$('#settings').onclick = renderSetup;
$('#launch').onclick = () => run(`Launching ${cur.name}`, () => invoke('launch', { mode: cur.mode_id }));

window.__TAURI__.event.listen('tauri://drag-drop', async (e) => {
  for (const p of e.payload.paths) await install(p).catch(() => {});
});

loadGames();
