import { expect, test, type Page } from '@playwright/test';

/** Mounts the recipe panel on the running app with the preview mock behind it. */
async function mount(page: Page, slash: string[]): Promise<void> {
  await page.goto('/');
  await page.evaluate(async slash => {
    const [{ mountRecipes }, { recipesCommand }] = await Promise.all([import('/src/recipes.ts'), import('/src/preview-recipes.ts')]);
    const root = document.createElement('div');
    root.id = 'recipes-root';
    root.style.cssText = 'position:fixed;inset:40px;z-index:99;background:#222;color:#eee;padding:16px';
    document.body.append(root);
    const w = window as any;
    w.__queued = [];
    mountRecipes(root, {
      projectId: 'p', provider: 'claude',
      call: async (command: string, args: any) => { const r = recipesCommand(command, args ?? {}); if (!r.handled) throw new Error(command); return r.result as any; },
      slashCommands: async () => slash,
      enqueue: async (text: string) => { w.__queued.push(text); },
    });
  }, slash);
}

test('dry-runs a recipe and queues its steps in order', async ({ page }) => {
  await mount(page, ['/review']);
  await page.locator('#recipes-root').getByRole('button', { name: /review-diff/ }).click();
  const table = page.locator('#recipes-root').getByRole('table', { name: 'Dry run for review-diff' });
  await expect(table.locator('.recipe-status')).toHaveText(['Supported', 'Supported', 'Supported', 'Manual']);
  await page.locator('#recipes-root').getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#recipes-root').getByRole('status')).toHaveText('Queued 4 messages.');
  const queued: string[] = await page.evaluate(() => (window as any).__queued);
  expect(queued[0]).toBe('Review the changed files and summarize risks.');
  expect(queued[1]).toBe('/review');
  expect(queued[2]).toContain('npm test');
  expect(queued[3]).toContain('confirm');
});

test('a missing command uses its fallback text, and a step without one blocks Run', async ({ page }) => {
  await mount(page, []);
  await page.locator('#recipes-root').getByRole('button', { name: /review-diff/ }).click();
  await expect(page.locator('.recipe-status').nth(1)).toHaveText('Fallback');
  await expect(page.locator('#recipes-root').getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  await page.locator('#recipes-root').getByRole('button', { name: /needs-ship/ }).click();
  await expect(page.locator('.recipe-status')).toHaveText('Blocked');
  await expect(page.locator('#recipes-root').getByRole('button', { name: 'Run', exact: true })).toBeDisabled();
});

test('creates and deletes a recipe, rejecting bad names', async ({ page }) => {
  await mount(page, []);
  await page.locator('#recipes-root').getByRole('button', { name: 'New recipe' }).click();
  const json = page.locator('#recipes-root').getByRole('textbox', { name: 'Recipe JSON' });
  await json.fill('{"name":"Bad Name","steps":[{"kind":"prompt","text":"x"}]}');
  await page.locator('#recipes-root').getByRole('button', { name: 'Save' }).click();
  await expect(page.locator('#recipes-root').getByRole('status')).toContainText('Recipe names use');
  await json.fill('{"name":"mine","description":"","steps":[{"kind":"prompt","text":"x"}]}');
  await page.locator('#recipes-root').getByRole('button', { name: 'Save' }).click();
  await expect(page.locator('#recipes-root').getByRole('button', { name: /mine/ })).toBeVisible();
  await page.locator('#recipes-root').getByRole('button', { name: 'Delete' }).click();
  await expect(page.locator('#recipes-root').getByRole('button', { name: /mine/ })).toHaveCount(0);
});
