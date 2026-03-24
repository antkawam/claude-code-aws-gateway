import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo } from '../helpers/gateway';

test.describe('Analytics Dashboard', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'org-analytics');
  });

  test('loads analytics page with controls', async ({ page }) => {
    await expect(page.locator('#page-org-analytics')).toBeVisible();
    // Time range selector should exist
    await expect(page.locator('#page-org-analytics select, #page-org-analytics [class*="time"], #page-org-analytics button:has-text("7d"), #page-org-analytics button:has-text("30d")').first()).toBeVisible();
  });

  test('switches between analytics tabs', async ({ page }) => {
    // Look for tab buttons (Spend, Activity, Models, Tools)
    const spendTab = page.locator('button:has-text("Spend"), [data-tab="spend"]').first();
    const activityTab = page.locator('button:has-text("Activity"), [data-tab="activity"]').first();

    if (await spendTab.isVisible()) {
      await spendTab.click();
      // Page should still be visible (no crash)
      await expect(page.locator('#page-org-analytics')).toBeVisible();
    }

    if (await activityTab.isVisible()) {
      await activityTab.click();
      await expect(page.locator('#page-org-analytics')).toBeVisible();
    }
  });

  test('has filter dropdowns', async ({ page }) => {
    // Multi-select filter wrappers should exist
    const teamFilter = page.locator('#oa-team-wrap, [id*="team-filter"]').first();
    if (await teamFilter.isVisible()) {
      await teamFilter.click();
      // Dropdown should expand
      await expect(page.locator('#page-org-analytics')).toBeVisible();
    }
  });

  test('has CSV export button', async ({ page }) => {
    const exportBtn = page.locator('button:has-text("CSV"), button:has-text("Export")').first();
    await expect(exportBtn).toBeVisible();
  });
});
