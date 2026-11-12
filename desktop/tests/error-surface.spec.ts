import { expect, test } from '@playwright/test';

test('unhandled rejections surface in the error toast', async ({ page }) => {
  await page.goto('/?view=launcher');
  await expect(page.locator('.launcher-task')).toHaveCount(4);
  await page.evaluate(() => { void Promise.reject(new Error('preview rejection')); });
  const toast = page.locator('.error-toast');
  await expect(toast).toBeVisible();
  await expect(toast).toContainText('preview rejection');
  await page.locator('.toast-dismiss').click();
  await expect(toast).toBeHidden();
});

test('reportError offers a retry action that re-runs the work', async ({ page }) => {
  await page.goto('/?view=launcher');
  await expect(page.locator('.launcher-task')).toHaveCount(4);
  await page.evaluate(() => {
    const win = window as any;
    win.__retryRan = false;
    void import('/src/error-surface.ts').then(({ reportError }) =>
      reportError(new Error('retry me'), { retry: () => { win.__retryRan = true; } }));
  });
  const toast = page.locator('.error-toast');
  await expect(toast).toBeVisible();
  await expect(toast).toContainText('retry me');
  await expect(page.locator('.toast-retry')).toBeVisible();
  await page.locator('.toast-retry').click();
  expect(await page.evaluate(() => (window as any).__retryRan)).toBe(true);
  await expect(toast).toBeHidden();
});
