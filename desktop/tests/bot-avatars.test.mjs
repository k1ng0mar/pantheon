import test from 'node:test';
import assert from 'node:assert/strict';
import { avatarPicker, botAvatarHtml, MARKS, markSvg } from '../src/bot-avatars.ts';

test('ten temple marks, each its own color, each a compact id-free SVG', () => {
  assert.deepEqual(MARKS.map(p => p.id), Array.from({ length: 10 }, (_, i) => `temple:${i}`));
  assert.equal(new Set(MARKS.map(p => p.color)).size, 10);
  for (const p of MARKS) {
    const svg = markSvg(p.id);
    assert.match(svg, /^<svg viewBox="0 0 64 64"/);
    assert.ok(svg.includes(p.color) && !/\sid=/.test(svg));
  }
  assert.equal(markSvg('temple:10'), '');
});

test('avatars render temple marks, uploaded images, legacy glyphs and escaped initials', () => {
  assert.match(botAvatarHtml('temple:3', 'Ana', 'x'), /^<span class="bot-avatar has-art x" aria-hidden="true"><svg/);
  const png = 'data:image/png;base64,iVBORw0KGgo=';
  assert.ok(botAvatarHtml(png, 'Ana').includes(`<img alt="" src="${png}">`));
  assert.ok(!botAvatarHtml('data:image/png;base64,"><script>', 'Ana').includes('<script>'));
  assert.equal(botAvatarHtml('🐼', 'Ana'), '<span class="bot-avatar" aria-hidden="true">🐼</span>');
  assert.equal(botAvatarHtml('', '<b>'), '<span class="bot-avatar" aria-hidden="true">&#60;</span>');
});

test('the picker marks the current choice and shows an uploaded image as its own swatch', () => {
  const html = avatarPicker('temple:2');
  assert.equal(html.match(/data-avatar-choice="temple:/g).length, 10);
  assert.ok(html.includes('data-avatar-choice="temple:2" aria-pressed="true" aria-label="Coral mark"'));
  assert.ok(html.includes('role="radiogroup" aria-label="Icon"') && html.includes('data-avatar-upload'));
  assert.ok(!html.includes('data-avatar-choice="custom"'));
  assert.ok(avatarPicker('data:image/png;base64,AAAA').includes('data-avatar-choice="custom" aria-pressed="true"'));
});
