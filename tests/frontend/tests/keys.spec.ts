import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo } from '../helpers/gateway';

test.describe('Virtual Key Lifecycle', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'keys');
  });

  test('creates a new virtual key', async ({ page }) => {
    // Open create key modal
    await page.click('button:has-text("Create Key")');
    await expect(page.locator('#modal-create-key')).toBeVisible();

    // Fill optional name and submit
    await page.fill('#mk-name', 'test-key-playwright');
    await page.click('#modal-create-key button:has-text("Create Key")');

    // Key banner should appear with sk-proxy- prefix
    await expect(page.locator('#new-key-banner')).toBeVisible({ timeout: 5_000 });
    const keyValue = await page.locator('#new-key-value').textContent();
    expect(keyValue).toMatch(/^sk-proxy-/);
  });

  test('shows key in the keys table after creation', async ({ page }) => {
    // Create a key
    await page.click('button:has-text("Create Key")');
    await page.fill('#mk-name', 'table-test-key');
    await page.click('#modal-create-key button:has-text("Create Key")');
    await expect(page.locator('#new-key-banner')).toBeVisible({ timeout: 5_000 });

    // Key should be in the table
    await expect(page.locator('#keys-table')).toContainText('table-test-key');
  });

  test('revokes a virtual key', async ({ page }) => {
    // Create a key first
    await page.click('button:has-text("Create Key")');
    await page.fill('#mk-name', 'revoke-test-key');
    await page.click('#modal-create-key button:has-text("Create Key")');
    await expect(page.locator('#new-key-banner')).toBeVisible({ timeout: 5_000 });

    // Find and click revoke button for the new key
    const revokeBtn = page.locator('#keys-table tr:has-text("revoke-test-key") button:has-text("Revoke")');
    await revokeBtn.click();

    // After revoke, the key should no longer have a Revoke button
    await expect(revokeBtn).toBeHidden({ timeout: 5_000 });
  });

  test('displays copy button for new key', async ({ page }) => {
    await page.click('button:has-text("Create Key")');
    await page.fill('#mk-name', 'copy-test');
    await page.click('#modal-create-key button:has-text("Create Key")');
    await expect(page.locator('#new-key-banner')).toBeVisible({ timeout: 5_000 });
    await expect(page.locator('#new-key-banner button:has-text("Copy")')).toBeVisible();
  });
});
