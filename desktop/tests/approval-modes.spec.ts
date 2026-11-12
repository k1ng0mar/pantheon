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

test('offers Approve for me to Claude Code and keeps it with the task', async ({ page }) => {
  const trigger = page.locator('#mode-picker-trigger');
  const menu = page.getByRole('dialog', { name: 'Permissions', exact: true });
  await trigger.click();
  await expect(menu.getByRole('menuitemradio')).toHaveCount(5);
  await menu.getByRole('menuitemradio', { name: 'Approve for me', exact: true }).click();
  await expect(trigger).toHaveText('Approve for me');
  await expect(trigger).toHaveAttribute('title', /auto mode/);
  await expect.poll(() => savedPermission(page)).toBe('review');
  await page.locator('.task-item[data-id="session-4"]').click();
  await page.locator('.task-item[data-id="session-1"]').click();
  await expect(trigger).toHaveText('Approve for me');
  await trigger.click();
  await expect(menu.getByRole('menuitemradio', { name: 'Approve for me', exact: true })).toHaveAttribute('aria-checked', 'true');
});

test('describes Plan mode investigation for Claude Code', async ({ page }) => {
  await page.locator('#mode-picker-trigger').click();
  await page.getByRole('dialog', { name: 'Permissions', exact: true }).getByRole('menuitemradio', { name: 'Plan mode', exact: true }).click();
  await expect(page.locator('#mode-picker-trigger')).toHaveAttribute('title', /web lookups run, even outside this project/);
  await expect.poll(() => savedPermission(page)).toBe('plan');
});
