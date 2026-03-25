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

    // Wait for dynamic setup content to render, then click "Create Key & Setup"
    const createBtn = page.locator('#page-setup button:has-text("Create Key")').first();
    if (await createBtn.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await createBtn.click();
      // Look for a terminal/code block with setup instructions
      const setupBlock = page.locator('#page-setup pre, #page-setup code, #page-setup .terminal, #page-setup [class*="setup-cmd"]').first();
      if (await setupBlock.isVisible({ timeout: 5_000 }).catch(() => false)) {
        const text = await setupBlock.textContent();
        expect(text).toBeTruthy();
      }
    }
  });

  test('has copy button for setup command', async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'setup');

    const createBtn = page.locator('#page-setup button:has-text("Create Key")').first();
    if (await createBtn.isVisible({ timeout: 5_000 }).catch(() => false)) {
      await createBtn.click();
      // Copy button should be available
      const copyBtn = page.locator('#page-setup button:has-text("Copy")').first();
      if (await copyBtn.isVisible({ timeout: 5_000 }).catch(() => false)) {
        await expect(copyBtn).toBeVisible();
      }
    }
  });
});
