// Run: npm test (node strips the types).
import assert from 'node:assert/strict';
import { dropSelected, range, stepSelected } from '../src/lib/order.ts';

const all = ['a', 'b', 'c', 'd', 'e'];
const s = (...n: string[]) => new Set(n);

// Each selected plugin swaps with its unselected neighbour; blocks move as one (C# BuildPluginOrderMovedByOne).
assert.deepEqual(stepSelected(all, all, s('b', 'd'), -1), ['b', 'a', 'd', 'c', 'e']);
assert.deepEqual(stepSelected(all, all, s('b', 'c'), 1), ['a', 'd', 'b', 'c', 'e']);
assert.deepEqual(stepSelected(all, all, s('a', 'c'), -1), ['a', 'c', 'b', 'd', 'e']);
assert.equal(stepSelected(all, all, s('a', 'b'), -1), null);
assert.equal(stepSelected(all, all, s(), 1), null);
// Filtered: steps among the visible rows; hidden ones stay put.
assert.deepEqual(stepSelected(all, ['a', 'c', 'e'], s('e'), -1), ['a', 'b', 'e', 'd', 'c']);

// Drag: the selection, in list order, lands before the drop target.
assert.deepEqual(dropSelected(all, s('d', 'a'), 'c'), ['b', 'a', 'd', 'c', 'e']);
assert.deepEqual(dropSelected(all, s('b'), null), ['a', 'c', 'd', 'e', 'b']);
// Dropped on a selected row: before the next unselected one.
assert.deepEqual(dropSelected(all, s('b', 'c'), 'c'), ['a', 'b', 'c', 'd', 'e']);

assert.deepEqual(range(all, 'd', 'b'), ['b', 'c', 'd']);
assert.deepEqual(range(all, null, 'c'), ['c']);
console.log('order ok');
