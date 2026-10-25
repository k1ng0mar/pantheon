import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

// State→UI tripwire (plan doc, state-coverage audit): every AgentEvent kind
// the backend can emit must be handled by the renderer's event pump — or be
// listed here as explicitly inert, with a reason. A new kind added to the
// union without handling fails this test instead of shipping as a silent
// state (the class of bug where an agent waits and nothing on screen moves).

const INERT = new Map([
  // 'example-kind': 'why no visible UI is required',
]);

test('every AgentEvent kind reaches the renderer event pump', async () => {
  const types = await readFile(new URL('../src/types.ts', import.meta.url), 'utf8');
  const block = types.slice(Math.max(0, types.indexOf('interface AgentEvent')));
  const union = block.match(/kind:\s*((?:'[^']+'(?:\s*\|\s*)?)+)/);
  assert.ok(union, 'AgentEvent kind union not found in src/types.ts');
  const kinds = [...union[1].matchAll(/'([^']+)'/g)].map(match => match[1]);
  assert.ok(kinds.length >= 20, `implausibly few event kinds parsed: ${kinds.length}`);

  const main = await readFile(new URL('../src/main.ts', import.meta.url), 'utf8');
  const unhandled = kinds.filter(kind => !main.includes(`'${kind}'`));
  const missing = unhandled.filter(kind => !INERT.has(kind));
  assert.deepEqual(
    missing,
    [],
    `AgentEvent kinds with no renderer handling and no inert entry: ${missing.join(', ')}. ` +
    'Handle the kind in src/main.ts (visible UI or state change) or add it to INERT with a reason.',
  );
  // An inert entry for a kind that IS handled is stale documentation.
  const stale = [...INERT.keys()].filter(kind => kinds.includes(kind) && main.includes(`'${kind}'`));
  assert.deepEqual(stale, [], `INERT entries for handled kinds are stale: ${stale.join(', ')}`);
});
