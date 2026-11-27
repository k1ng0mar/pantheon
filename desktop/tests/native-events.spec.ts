import { expect, test, type Page } from '@playwright/test';

async function openPantheonTask(page: Page): Promise<void> {
  await page.route('**/src/preview.ts*', async route => {
    const response = await route.fetch();
    const source = await response.text();
    if (!source.includes('function agent(')) throw new Error('preview agent bridge moved');
    const body = `${source}\nwindow.__nativeEmit = event => agent(event);`;
    await route.fulfill({ response, body });
  });
  await page.goto('/');
  await page.locator('.task-item[data-id="session-2"]').click();
  await expect(page.locator('#prompt')).toBeEnabled();
}

test('native usage and uiDocument events reach the live turn', async ({ page }) => {
  await openPantheonTask(page);
  await page.evaluate(() => (window as any).__nativeEmit({
    sessionId: 'session-2', kind: 'turn_start', text: '',
  }));
  await page.evaluate(() => (window as any).__nativeEmit({
    sessionId: 'session-2', kind: 'usage_report', text: '',
    data: { inputTokens: 120, outputTokens: 30, totalTokens: 150, costUsd: 0.02 },
  }));
  await expect(page.locator('.live-usage')).toContainText('150 tokens this turn');
  await expect(page.locator('.live-usage')).toContainText('$0.02');

  await page.evaluate(() => (window as any).__nativeEmit({
    sessionId: 'session-2', kind: 'ui_document', text: '',
    data: {
      uiId: 'd-1', revision: 1,
      document: { version: 1, id: 'd-1', root: { id: 'r', type: 'markdown', text: 'Native document reached the client' } },
    },
  }));
  await expect(page.locator('.native-ui-document')).toContainText('Native document reached the client');
});
