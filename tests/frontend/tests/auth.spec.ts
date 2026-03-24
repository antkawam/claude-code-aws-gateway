import { test, expect } from '@playwright/test';
import { loginViaPortal, ADMIN_USER, ADMIN_PASS } from '../helpers/gateway';

test.describe('Admin Authentication', () => {
  test('shows login screen on initial visit', async ({ page }) => {
    await page.goto('/portal');
    await expect(page.locator('#auth-screen')).toBeVisible();
    await expect(page.locator('#app-shell')).toBeHidden();
  });

  test('logs in with valid admin credentials', async ({ page }) => {
    await loginViaPortal(page);
    await expect(page.locator('#app-shell')).toBeVisible();
    await expect(page.locator('#auth-screen')).toBeHidden();
    // Sidebar should be visible with nav items
    await expect(page.locator('.sidebar')).toBeVisible();
  });

  test('rejects invalid credentials', async ({ page }) => {
    await page.goto('/portal');
    await page.fill('#auth-username', 'admin');
    await page.fill('#auth-password', 'wrong-password');
    await page.click('button:has-text("Sign in")');
    // Should show error and stay on auth screen
    await expect(page.locator('#auth-error')).toBeVisible({ timeout: 5_000 });
    await expect(page.locator('#app-shell')).toBeHidden();
  });

  test('persists session across page reload', async ({ page }) => {
    await loginViaPortal(page);
    await expect(page.locator('#app-shell')).toBeVisible();
    // Reload the page
    await page.reload();
    // Should still be logged in (token in localStorage)
    await expect(page.locator('#app-shell')).toBeVisible({ timeout: 10_000 });
    await expect(page.locator('#auth-screen')).toBeHidden();
  });

  test('logout clears session and returns to login', async ({ page }) => {
    await loginViaPortal(page);
    await expect(page.locator('#app-shell')).toBeVisible();
    // Click logout button
    await page.click('.btn-logout');
    await expect(page.locator('#auth-screen')).toBeVisible({ timeout: 5_000 });
    await expect(page.locator('#app-shell')).toBeHidden();
    // Verify localStorage is cleared
    const token = await page.evaluate(() => localStorage.getItem('proxyApiKey'));
    expect(token).toBeNull();
  });
});
