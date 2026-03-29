import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo, getSessionToken, apiCall } from '../helpers/gateway';

test.describe('Web Search Admin Visibility', () => {
  test('portal shows Web Search nav item when mode is enabled (default)', async ({ page }) => {
    await loginViaPortal(page);

    // The Web Search nav item should be visible in the sidebar by default
    const wsNav = page.locator('.nav-item[data-page="websearch"]');
    await expect(wsNav).toBeVisible({ timeout: 5_000 });
    await expect(wsNav).toContainText('Web Search');
  });

  test('portal hides Web Search nav item when mode is disabled', async ({ page }) => {
    // Set websearch mode to "disabled" via the admin API
    const token = await getSessionToken();
    await apiCall(token, 'PUT', '/admin/websearch-mode', { mode: 'disabled' });

    // Login and check the portal
    await loginViaPortal(page);

    // The Web Search nav item should be hidden when mode is "disabled"
    const wsNav = page.locator('.nav-item[data-page="websearch"]');
    await expect(wsNav).toBeHidden({ timeout: 5_000 });
  });

  test('portal shows Web Search nav item when mode is global', async ({ page }) => {
    // Set websearch mode to "global" via the admin API
    const token = await getSessionToken();
    await apiCall(token, 'PUT', '/admin/websearch-mode', {
      mode: 'global',
      provider: { provider_type: 'tavily', api_key: 'test-key' },
    });

    await loginViaPortal(page);

    // The Web Search nav item should be visible in global mode
    const wsNav = page.locator('.nav-item[data-page="websearch"]');
    await expect(wsNav).toBeVisible({ timeout: 5_000 });
    await expect(wsNav).toContainText('Web Search');
  });

  test('portal re-shows Web Search nav after switching from disabled to enabled', async ({ page }) => {
    const token = await getSessionToken();

    // First disable
    await apiCall(token, 'PUT', '/admin/websearch-mode', { mode: 'disabled' });

    // Then re-enable
    await apiCall(token, 'PUT', '/admin/websearch-mode', { mode: 'enabled' });

    await loginViaPortal(page);

    // Should be visible again
    const wsNav = page.locator('.nav-item[data-page="websearch"]');
    await expect(wsNav).toBeVisible({ timeout: 5_000 });
  });

  test('settings API returns websearch_mode for portal consumption', async ({ page }) => {
    // This test verifies the data contract between backend and portal.
    // The portal calls GET /admin/settings on load and uses the response
    // to determine UI visibility. websearch_mode must be in that response.
    const token = await getSessionToken();
    const settings = await apiCall(token, 'GET', '/admin/settings');

    // The settings response must include websearch_mode
    expect(settings).toHaveProperty('websearch_mode');
    expect(['enabled', 'disabled', 'global']).toContain(settings.websearch_mode);
  });
});
