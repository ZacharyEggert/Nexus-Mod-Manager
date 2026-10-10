// Plugin list moves for a multi-selection. Pure: names in, names out.

/** Move every selected name one step among `visible` (C# `BuildPluginOrderMovedByOne`): each
 *  swaps with its unselected neighbour, so blocks move as one. null when nothing moves. */
export function stepSelected(names: string[], visible: string[], sel: Set<string>, delta: -1 | 1): string[] | null {
  const v = [...visible];
  let changed = false;
  const swap = (i: number, j: number) => {
    [v[i], v[j]] = [v[j], v[i]];
    changed = true;
  };
  if (delta < 0) {
    for (let i = 1; i < v.length; i++) if (sel.has(v[i]) && !sel.has(v[i - 1])) swap(i, i - 1);
  } else {
    for (let i = v.length - 2; i >= 0; i--) if (sel.has(v[i]) && !sel.has(v[i + 1])) swap(i, i + 1);
  }
  if (!changed) return null;
  // Write the visible rows back into their slots; hidden rows keep theirs.
  const shown = new Set(visible);
  let k = 0;
  return names.map((n) => (shown.has(n) ? v[k++] : n));
}

/** Move the selected names, in list order, to just before `before` (null = end). */
export function dropSelected(names: string[], sel: Set<string>, before: string | null): string[] {
  const moving = names.filter((n) => sel.has(n));
  const rest = names.filter((n) => !sel.has(n));
  // Dropped on a selected row: land before the next unselected one.
  let target = before;
  if (target !== null && sel.has(target)) target = names.slice(names.indexOf(target)).find((n) => !sel.has(n)) ?? null;
  const i = target === null ? rest.length : rest.indexOf(target);
  return [...rest.slice(0, i), ...moving, ...rest.slice(i)];
}

/** Names from `a` to `b` inclusive, in `list` order; just `b` without an anchor. */
export function range(list: string[], a: string | null, b: string): string[] {
  const i = a === null ? -1 : list.indexOf(a);
  const j = list.indexOf(b);
  if (i < 0) return [b];
  return list.slice(Math.min(i, j), Math.max(i, j) + 1);
}
