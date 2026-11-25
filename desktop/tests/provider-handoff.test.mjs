import test from 'node:test';
import assert from 'node:assert/strict';
import { historicalProvider } from '../src/provider-handoff.ts';

test('saved author metadata survives a page without its future handoff', () => {
  const response = { id: 4, sessionId: 'task', role: 'assistant', content: 'Earlier answer', createdAt: 1, authorProvider: 'pantheon' };
  assert.equal(historicalProvider(response, [response], 'pantheon'), 'pantheon');
  assert.equal(historicalProvider({ ...response, authorProvider: 'javascript:alert(1)' }, [response], 'pantheon'), 'pantheon');
});

test('legacy pages infer only handoffs belonging to the same task', () => {
  const response = { id: 4, sessionId: 'task', role: 'assistant', content: 'Earlier answer', createdAt: 1 };
  const handoff = { id: 5, sessionId: 'other', role: 'handoff', content: JSON.stringify({fromProvider:'pantheon',toProvider:'pantheon',summary:'Same agent both sides'}), createdAt: 2 };
  assert.equal(historicalProvider(response, [response, handoff], 'pantheon'), 'pantheon');
  assert.equal(historicalProvider(response, [response, {...handoff,sessionId:'task'}], 'pantheon'), 'pantheon');
  assert.equal(historicalProvider({...response,id:-1}, [response, {...handoff,sessionId:'task'}], 'pantheon'), 'pantheon');
});
