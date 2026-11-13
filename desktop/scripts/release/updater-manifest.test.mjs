import { strict as assert } from 'node:assert';
import { test } from 'node:test';
import { promises as fs } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const src = dirname(fileURLToPath(import.meta.url));
const dir = join(src, '.tmp-manifest-' + Date.now());

test('manifest is written with the given version and notes', async () => {
  await fs.mkdir(dir, { recursive: true });
  await fs.writeFile(join(dir, 'Pantheon-Linux-x86_64.AppImage.sig'), 'sig-x64\n');
  await fs.writeFile(join(dir, 'notes.md'), ' Release notes here\n');
  const um = await import('./updater-manifest.mjs');
  await um.main(dir, '0.3.1', join(dir, 'notes.md'));
  const manifest = JSON.parse(await fs.readFile(join(dir, 'latest.json'), 'utf8'));
  assert.equal(manifest.version, '0.3.1');
  assert.equal(manifest.notes, 'Release notes here');
  assert.equal(Object.keys(manifest.platforms).length, 1);
  assert.equal(manifest.platforms['linux-x86_64'].signature, 'sig-x64');
  await fs.rm(dir, { recursive: true, force: true });
});
