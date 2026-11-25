import { expect, test, type Page } from '@playwright/test';

const previewBody = (source: string) => source + '\nwindow.__pantheonPreviewUrl = import.meta.url;\n';
const savedPermission = (page: Page) => page.evaluate(async () => {
  const { invoke } = await import((window as any).__pantheonPreviewUrl);
  const id = document.querySelector<HTMLElement>('.task-item.selected')?.dataset.id;
  return id ? (await invoke('get_task', { sessionId: id }))?.mode : undefined;
});

test.beforeEach(async ({ page }) => {
  await page.route('**/src/preview.ts*', async route => {
    const response = await route.fetch();
    await route.fulfill({ response, body: previewBody(await response.text()) });
  });
  await page.goto('/');
  await expect(page.locator('.task-item.selected .task-title')).toHaveText('Make search feel instant');
});

// Pantheon's ACP server advertises exactly two modes, `build` and `plan`, and
// has no agent-reviewed or edit-only approval mode. Offering either one meant
// the mode had no policy behind it or failed at launch - audit DESKTOP-2.
test('offers only the access modes this harness can actually be put into', async ({ page }) => {
  const trigger = page.locator('#mode-picker-trigger');
  const menu = page.getByRole('dialog', { name: 'Permissions', exact: true });
  await trigger.click();
  await expect(menu.getByRole('menuitemradio')).toHaveCount(3);
  for (const name of ['Standard', 'Plan mode', 'Full access']) {
    await expect(menu.getByRole('menuitemradio', { name, exact: true })).toHaveCount(1);
  }
  await expect(menu.getByRole('menuitemradio', { name: 'Accept edits', exact: true })).toHaveCount(0);
  await expect(menu.getByRole('menuitemradio', { name: 'Approve for me', exact: true })).toHaveCount(0);
});

test('keeps the chosen mode with the task', async ({ page }) => {
  const trigger = page.locator('#mode-picker-trigger');
  const menu = page.getByRole('dialog', { name: 'Permissions', exact: true });
  await trigger.click();
  await menu.getByRole('menuitemradio', { name: 'Plan mode', exact: true }).click();
  await expect(trigger).toHaveText('Plan mode');
  await expect.poll(() => savedPermission(page)).toBe('plan');
  await page.locator('.task-item[data-id="session-4"]').click();
  await page.locator('.task-item[data-id="session-1"]').click();
  await expect(trigger).toHaveText('Plan mode');
  await trigger.click();
  await expect(menu.getByRole('menuitemradio', { name: 'Plan mode', exact: true })).toHaveAttribute('aria-checked', 'true');
});

test('describes Plan mode investigation', async ({ page }) => {
  await page.locator('#mode-picker-trigger').click();
  await page.getByRole('dialog', { name: 'Permissions', exact: true }).getByRole('menuitemradio', { name: 'Plan mode', exact: true }).click();
  await expect(page.locator('#mode-picker-trigger')).toHaveAttribute('title', /proposes a plan/);
  await expect.poll(() => savedPermission(page)).toBe('plan');
});
