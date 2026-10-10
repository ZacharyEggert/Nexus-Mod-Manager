// App-wide UI state: the status line and the running-task rows.

import * as api from './api';

export interface TaskRow {
  id: string;
  label: string;
  progress: api.Progress | null;
  /** Cancel pressed, or rolling back: the button is disabled. */
  cancelling: boolean;
}

export const ui = $state({
  status: { msg: '', err: false },
  tasks: [] as TaskRow[],
});

export function status(msg: string, err = false) {
  ui.status = { msg, err };
}

/** Run `fn` with "label…" / "label — done" / "label failed: …" in the status line; rethrows. */
export async function run<T>(label: string, fn: () => Promise<T>): Promise<T> {
  status(label + '…');
  try {
    const r = await fn();
    status(label + ' — done');
    return r;
  } catch (e) {
    if (e === api.CANCELLED) status(label + ' — cancelled');
    else status(`${label} failed: ${e}`, true);
    throw e;
  }
}

/** Run a backend task with a progress row + Cancel button, reported like `run`. */
export function runTask<T>(label: string, c: api.TaskCall<T>): Promise<T> {
  return run(label, () => {
    const { id, done } = api.start(c, (p) => {
      const row = ui.tasks.find((t) => t.id === id);
      if (!row) return;
      row.progress = p;
      if (p.step === 'Rolling back') row.cancelling = true;
    });
    ui.tasks.push({ id, label, progress: null, cancelling: false });
    return done.finally(() => {
      ui.tasks = ui.tasks.filter((t) => t.id !== id);
    });
  });
}

export function cancelTask(row: TaskRow) {
  row.cancelling = true;
  api.cancel(row.id);
}

api.onQuitting(() => {
  ui.tasks.forEach((t) => (t.cancelling = true));
  status('Undoing unfinished changes before closing…');
});

/** Link modes with how they read after "…place mod files by". */
export const LINK_MODES: [api.LinkMode, string][] = [
  ['auto', 'hardlink, or copy if not possible'],
  ['hardlink', 'hardlink only'],
  ['symlink', 'symlink'],
  ['copy', 'copying'],
];
