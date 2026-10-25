import { expect, test } from '@playwright/test';

test('a named connection is added in Settings, chosen for a task in the model picker, and kept', async ({ page }) => {
  const errors: string[] = []; page.on('pageerror', error => errors.push(error.message));
  await page.goto('/');
  await expect(page.locator('.task-item.selected .task-title')).toHaveText('Make search feel instant');
  const taskId = await page.locator('.task-item.selected').getAttribute('data-id');
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await page.getByRole('region', { name: 'Settings', exact: true }).getByRole('tab', { name: 'Providers', exact: true }).click();
  const card = page.locator('#named-connections');
  await card.getByRole('button', { name: 'Add connection', exact: true }).click();
  await card.getByLabel('Harness', { exact: true }).selectOption('claude');
  await expect(card.locator('[data-config-var]')).toHaveText('(sets CLAUDE_CONFIG_DIR)');
  await card.getByLabel('Name', { exact: true }).fill('Claude Work');
  await card.getByLabel('Config folder').fill('/Users/sample/.claude-work');
  await card.getByRole('button', { name: 'Add variable', exact: true }).click();
  await card.getByLabel('Variable name').fill('ANTHROPIC_API_KEY');
  await card.getByLabel('Value').fill('fixture-secret');
  await card.getByLabel('Secret').check();
  await expect(card.getByLabel('Value')).toHaveAttribute('type', 'password');
  await card.getByRole('button', { name: 'Save connection', exact: true }).click();
  await expect(card.getByRole('status')).toHaveText('Connection saved. Choose it in the model picker.');
  await expect(card.locator('.named-connection')).toHaveText(/Claude Work\s*Claude Code/);
  // Editing shows the secret by name only; its value never comes back.
  await card.getByRole('button', { name: 'Edit Claude Work', exact: true }).click();
  await expect(card.getByLabel('Variable name')).toHaveValue('ANTHROPIC_API_KEY');
  await expect(card.getByLabel('Value')).toHaveValue('');
  await card.getByRole('button', { name: 'Cancel', exact: true }).click();
  await page.keyboard.press('Escape');

  await page.locator('#model-picker-trigger').click();
  const picker = page.getByRole('dialog', { name: 'Harness and model', exact: true });
  await expect(picker.getByRole('region', { name: 'Claude Work', exact: true })).toBeVisible();
  await picker.getByRole('button', { name: 'Claude Work · Claude Sonnet 5', exact: true }).click();
  await expect(page.locator('#model-picker-trigger')).toContainText('Claude Work · Claude Sonnet 5');

  // Another task keeps the harness's own setup; returning shows the connection again.
  await page.locator('.task-item:not(.selected)').first().click();
  await expect(page.locator('#model-picker-trigger')).not.toContainText('Claude Work');
  await page.locator(`.task-item[data-id="${taskId}"]`).click();
  await expect(page.locator('#model-picker-trigger')).toContainText('Claude Work · Claude Sonnet 5');
  await page.locator('#model-picker-trigger').click();
  await expect(picker.locator('.model-menu-option[aria-current="true"]')).toHaveAttribute('aria-label', 'Claude Work · Claude Sonnet 5');
  // The plain harness group clears it.
  await picker.getByRole('button', { name: 'Claude Code · Claude Sonnet 5', exact: true }).click();
  await expect(page.locator('#model-picker-trigger')).not.toContainText('Claude Work');
  expect(errors).toEqual([]);
});
