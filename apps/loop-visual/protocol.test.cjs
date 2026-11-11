'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { safeState, LineDecoder } = require('./protocol.cjs');

test('JSON Lines decoder handles split and batched events without dropping Unicode', () => {
  const values = [], errors = [];
  const decoder = new LineDecoder((value) => values.push(value), (error) => errors.push(error));
  decoder.push('{"type":"rea');
  decoder.push('dy"}\n{"text":"λ"}\n');
  assert.deepEqual(values, [{ type: 'ready' }, { text: 'λ' }]);
  assert.equal(errors.length, 0);
});
test('decoder rejects malformed and oversized events and continues', () => {
  const values = [], errors = [];
  const decoder = new LineDecoder((value) => values.push(value), (error) => errors.push(error), 20);
  decoder.push('broken\n');
  decoder.push('x'.repeat(21));
  decoder.push('{"ok":true}\n');
  assert.deepEqual(values, [{ ok: true }]);
  assert.equal(errors.length, 2);
});
test('scene state accepts controls but drops executable or excessive messages', () => {
  assert.deepEqual(safeState({ selected: 'node-a', controls: { length: 16, mode: 'linear' }, command: 'shell' }),
    { selected: 'node-a', controls: { length: 16, mode: 'linear' } });
  assert.equal(safeState({ controls: { bad: () => 1 } }), null);
  assert.equal(safeState({ controls: { large: 'x'.repeat(9000) } }), null);
  assert.equal(safeState(JSON.parse('{"controls":{"__proto__":{"polluted":true}}}')), null);
});
