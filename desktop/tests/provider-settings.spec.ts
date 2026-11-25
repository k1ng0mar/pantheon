import { expect, test, type Page } from '@playwright/test';

const previewBody = (source: string) => source + '\nwindow.__pantheonPreviewUrl = import.meta.url;\n';

test.beforeEach(async ({ page }) => {
  await page.route('**/src/preview.ts*', async route => {
    const response = await route.fetch();
    await route.fulfill({ response, body: previewBody(await response.text()) });
  });
  await page.goto('/');
  await expect(page.locator('.task-item.selected .task-title')).toHaveText('Make search feel instant');
});

const settings = (page: Page) => page.getByRole('region', { name: 'Settings', exact: true });

async function openAgents(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'Settings' }).click();
  await settings(page).getByRole('tab', { name: 'Agents', exact: true }).click();
  const row = settings(page).locator('.provider-diagnostics details[data-provider="pantheon"]');
  await expect(row).toBeVisible();
  await row.locator('summary').click();
}

async function openProviders(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'Settings' }).click();
  await settings(page).getByRole('tab', { name: 'Providers', exact: true }).click();
  await expect(settings(page).locator('.connection-card[data-provider="pantheon"]')).toBeVisible();
}

// One agent ships with this desktop, so the panel is one card. Anything that
// claimed to configure another harness would be configuring nothing.
test('the providers panel offers only the agent this desktop ships', async ({ page }) => {
  await openProviders(page);
  const panel = settings(page).locator('#settings-panel-providers');
  await expect(panel.locator('.connection-card')).toHaveCount(1);
  await expect(panel.locator('.connection-card h3')).toContainText('Pantheon');
  await expect(panel.getByText('Pantheon reads its model and credentials from its own runtime, so there is nothing to paste here.', { exact: true })).toBeVisible();
  for (const provider of ['claude', 'codex', 'kimi', 'opencode', 'glm', 'mimo', 'copilot', 'antigravity', 'grok']) {
    await expect(panel.locator(`[data-provider="${provider}"]`)).toHaveCount(0);
    await expect(panel.locator(`.provider-logo[data-provider="${provider}"]`)).toHaveCount(0);
  }
  await expect(panel.locator('input[type="password"]')).toHaveCount(0);
});

test('clearing the executable hands discovery back and says so', async ({ page }) => {
  await openProviders(page);
  const form = settings(page).locator('.provider-executable-form');
  const input = form.getByLabel('Pantheon executable', { exact: true });
  await expect(input).toHaveValue('');
  await expect(form.locator('[data-executable-clear]')).toBeDisabled();
  await input.fill('/opt//pantheon/bin/pantheon');
  await form.getByRole('button', { name: 'Apply', exact: true }).click();
  await expect.poll(() => input.inputValue()).toBe('/opt/pantheon/bin/pantheon');
  await expect(form.locator('.provider-executable-status')).toHaveText('Saved. New tasks launch this executable.');
  await expect(form.locator('[data-executable-clear]')).toBeEnabled();

  await form.locator('[data-executable-clear]').click();
  await expect.poll(() => input.inputValue()).toBe('');
  await expect(form.locator('.provider-executable-status')).toHaveText('Cleared. Pantheon is found automatically again.');
  await expect(form.locator('[data-executable-clear]')).toBeDisabled();
});

test('a rejected executable keeps the saved one and shows the reason', async ({ page }) => {
  await openProviders(page);
  const form = settings(page).locator('.provider-executable-form');
  const input = form.getByLabel('Pantheon executable', { exact: true });
  await input.fill('/opt/pantheon/bin/pantheon');
  await form.getByRole('button', { name: 'Apply', exact: true }).click();
  await expect.poll(() => input.inputValue()).toBe('/opt/pantheon/bin/pantheon');
  await input.fill('/opt/pantheon/bin/not-pantheon');
  await form.getByRole('button', { name: 'Apply', exact: true }).click();
  await expect(form.locator('.provider-executable-status')).toHaveText('That file is not executable.');
  await input.fill('/opt/pantheon/bin/pantheon');
  await form.getByRole('button', { name: 'Apply', exact: true }).click();
  await expect.poll(() => input.inputValue()).toBe('/opt/pantheon/bin/pantheon');
});

test('the saved executable is read back when the panel reopens', async ({ page }) => {
  await openProviders(page);
  const form = settings(page).locator('.provider-executable-form');
  await form.getByLabel('Pantheon executable', { exact: true }).fill('/opt/pantheon/bin/pantheon');
  await form.getByRole('button', { name: 'Apply', exact: true }).click();
  await expect(form.locator('.provider-executable-status')).toHaveText('Saved. New tasks launch this executable.');
  await page.keyboard.press('Escape');
  await openProviders(page);
  await expect(settings(page).locator('.provider-executable-form').getByLabel('Pantheon executable', { exact: true })).toHaveValue('/opt/pantheon/bin/pantheon');
});

test('the connection check reports readiness instead of assuming installation is enough', async ({ page }) => {
  await openAgents(page);
  const readiness = settings(page).locator('.provider-readiness');
  await expect(readiness.getByRole('status')).toHaveText('Connection not checked');
  await readiness.getByRole('button', { name: 'Check connection', exact: true }).click();
  await expect(readiness.getByRole('status')).toHaveText('Connection ready');
  await expect(readiness.locator('.provider-readiness-detail')).toHaveText('Sample connection check. No prompt sent.');
  await expect(readiness.getByRole('button')).toHaveText('Check again');
});

test('installed agents list the version, location and ACP readiness', async ({ page }) => {
  await page.getByRole('button', { name: 'Settings' }).click();
  await settings(page).getByRole('tab', { name: 'Agents', exact: true }).click();
  const rows = settings(page).locator('.provider-diagnostics details');
  await expect(rows).toHaveCount(1);
  const row = rows.first();
  await expect(row).toHaveAttribute('data-provider', 'pantheon');
  await expect(row.locator('strong')).toHaveText('Pantheon');
  await row.locator('summary').click();
  await expect(row.locator('.settings-provider-version')).toHaveText('Sample provider');
  await expect(row.locator('code')).toHaveText('/sample/bin/pantheon');
  await expect(row.locator('.settings-installed')).toHaveText('ACP installed');
});
