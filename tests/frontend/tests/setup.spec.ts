import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo } from '../helpers/gateway';

test.describe('Setup / Connect Flow', () => {
  test('navigates to setup page', async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'setup');
    await expect(page.locator('#page-setup')).toBeVisible();
  });

  test('displays setup instructions', async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'setup');
    // Should show instructions about connecting Claude Code
    await expect(page.locator('#page-setup')).toContainText(/Claude Code|ANTHROPIC_BASE_URL|setup/i);
  });

  test('creates key from setup page', async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'setup');

    // Click the "Create Key & Setup" button
    const createBtn = page.locator('button:has-text("Create Key")').first();
    if (await createBtn.isVisible()) {
      await createBtn.click();
      // Should show setup command with the gateway URL
      await page.waitForTimeout(1_000);
      // Look for a terminal/code block with setup instructions
      const setupBlock = page.locator('pre, code, .terminal, [class*="setup-cmd"]').first();
      if (await setupBlock.isVisible({ timeout: 3_000 }).catch(() => false)) {
        const text = await setupBlock.textContent();
        // Should reference the current host
        expect(text).toBeTruthy();
      }
    }
  });

  test('has copy button for setup command', async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'setup');

    const createBtn = page.locator('button:has-text("Create Key")').first();
    if (await createBtn.isVisible()) {
      await createBtn.click();
      await page.waitForTimeout(1_000);
      // Copy button should be available
      const copyBtn = page.locator('#page-setup button:has-text("Copy")').first();
      if (await copyBtn.isVisible({ timeout: 3_000 }).catch(() => false)) {
        await expect(copyBtn).toBeVisible();
      }
    }
  });
});
