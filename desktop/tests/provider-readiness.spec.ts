import { expect, test } from '@playwright/test';

const previewBody = (source: string) => source + '\nwindow.__pantheonPreviewUrl = import.meta.url;\n';

// A check that fails must say so and stay retryable; the panel is the only
// place a launch problem can be explained before a task starts.
test.beforeEach(async ({ page }) => {
  await page.route('**/src/preview.ts*', async route => {
    const response = await route.fetch();
    let source = await response.text();
    source = source.replace(
      "case 'provider_check_connection': result =",
      "case 'provider_check_connection': if ((window as any).__failCheck) throw new Error('Pantheon’s ACP server exited before the handshake.'); result =",
    );
    await route.fulfill({ response, body: previewBody(source) });
  });
  await page.goto('/');
});

test('a failed connection check explains itself and can be retried', async ({ page }) => {
  const settings = page.getByRole('region', { name: 'Settings', exact: true });
  await page.getByRole('button', { name: 'Settings' }).click();
  await settings.getByRole('tab', { name: 'Agents', exact: true }).click();
  const row = settings.locator('.provider-diagnostics details[data-provider="pantheon"]');
  await row.locator('summary').click();
  const readiness = row.locator('.provider-readiness');
  await page.evaluate(() => { (window as any).__failCheck = true; });
  await readiness.getByRole('button', { name: 'Check connection', exact: true }).click();
  await expect(readiness.getByRole('status')).toHaveText('Could not check connection');
  await expect(readiness.locator('.provider-readiness-detail')).toContainText('ACP server exited');
  await page.evaluate(() => { (window as any).__failCheck = false; });
  await readiness.getByRole('button', { name: 'Check again', exact: true }).click();
  await expect(readiness.getByRole('status')).toHaveText('Connection ready');
});
