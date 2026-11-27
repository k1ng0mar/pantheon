import assert from 'node:assert/strict';
import test from 'node:test';
import { uiDocumentSpec } from '../src/ui-document.ts';

test('native ui documents map to renderer specs', () => {
  const event = {
    uiId: 'd-1',
    revision: 1,
    document: {
      version: 1,
      id: 'd-1',
      root: {
        id: 'r',
        type: 'layout',
        kind: 'stack',
        children: [
          { id: 'm', type: 'markdown', text: 'Deployed' },
          { id: 's', type: 'metrics', items: [{ label: 'Latency', value: '120 ms' }] },
        ],
      },
    },
  };
  const spec = uiDocumentSpec(event);
  assert.ok(spec);
  assert.deepEqual(JSON.parse(spec), {
    type: 'stack',
    children: [
      { type: 'markdown', text: 'Deployed' },
      { type: 'stats', items: [{ label: 'Latency', value: '120 ms', delta: '' }] },
    ],
  });
});

test('native ui documents reject wrong versions', () => {
  assert.equal(uiDocumentSpec({ document: { version: 2, id: 'd', root: {} } }), null);
});
