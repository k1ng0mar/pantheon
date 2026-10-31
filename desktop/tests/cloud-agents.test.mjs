import test from 'node:test';
import assert from 'node:assert/strict';

// The pure parts of cloud agents in the webview: "Run on" choices, harness support and stage text.
const { SETUP_CHOICE, busyStage, cloudChoice, cloudItems, cloudProvider, harnessRefusal, stageText } = await import('../src/cloud-agents.ts');

test('cloud choices round-trip and never collide with this computer or Connect devices', () => {
  assert.equal(cloudProvider(cloudChoice('e2b')), 'e2b');
  assert.equal(cloudProvider('local'), undefined);
  assert.equal(cloudProvider('dev-studio'), undefined);
});

test('the Cloud section lists connected providers only, then setup', () => {
  const items = cloudItems([{ provider: 'e2b', label: 'E2B', ready: true }, { provider: 'modal', label: 'Modal', ready: false }], cloudChoice('e2b'));
  assert.deepEqual(items.map(item => [item.id, item.label, Boolean(item.selected)]), [
    ['cloud:e2b', 'Cloud · E2B', true],
    [SETUP_CHOICE, 'Set up cloud providers…', false],
  ]);
  // Nothing connected: setup alone.
  assert.deepEqual(cloudItems([{ provider: 'modal', label: 'Modal', ready: false }], 'local').map(item => item.id), [SETUP_CHOICE]);
  assert.deepEqual(cloudItems([], 'local').map(item => item.id), [SETUP_CHOICE]);
});

// The desktop is Pantheon-only (39af97a made the app Pantheon-only): a cloud
// machine installs and runs Pantheon's own binary, never a third-party CLI
// harness. Only the Pantheon provider is accepted; every other id is refused
// with the one-line reason.
test('only the Pantheon runtime runs in the cloud', () => {
  assert.equal(harnessRefusal('pantheon', 'Pantheon'), '');
  for (const id of ['claude', 'glm', 'mimo', 'codex', 'opencode', 'kimi']) {
    assert.equal(harnessRefusal(id, id), `${id} can’t run on a cloud machine yet.`);
  }
  assert.equal(harnessRefusal('codex', 'Codex'), 'Codex can’t run on a cloud machine yet.');
});

test('stages read as short phrases; busy ones spin', () => {
  // Every harness installs Pantheon itself on the cloud machine, so the
  // installing text no longer varies by harness.
  assert.equal(stageText('installing', null, 'pantheon'), 'Installing Pantheon');
  assert.equal(stageText('installing', null, 'claude'), 'Installing Pantheon');
  assert.equal(stageText('error', 'E2B refused the key', 'pantheon'), 'Error: E2B refused the key');
  assert.equal(stageText('reconnecting', null, 'pantheon'), 'Reconnecting…');
  assert.equal(busyStage('uploading'), true);
  assert.equal(busyStage('ready'), false);
  assert.equal(busyStage('error'), false);
});
