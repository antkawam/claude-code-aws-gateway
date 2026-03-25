import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo } from '../helpers/gateway';

test.describe('Analytics Dashboard', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'org-analytics');
  });

  test('loads analytics page with controls', async ({ page }) => {
    await expect(page.locator('#page-org-analytics')).toBeVisible();
    // Time range selector should exist (custom dropdown with days label)
    await expect(page.locator('#oa-days-label')).toBeVisible();
  });

  test('switches between analytics tabs', async ({ page }) => {
    // Tab buttons live in #oa-tab-bar
    const spendTab = page.locator('#oa-tab-bar button:has-text("Spend")');
    const activityTab = page.locator('#oa-tab-bar button:has-text("Activity")');

    await spendTab.click();
    await expect(page.locator('#page-org-analytics')).toBeVisible();

    await activityTab.click();
    await expect(page.locator('#page-org-analytics')).toBeVisible();
  });

  test('has filter dropdowns', async ({ page }) => {
    // Multi-select team filter wrapper
    const teamFilter = page.locator('#oa-team-wrap');
    await expect(teamFilter).toBeVisible();
    await teamFilter.click();
    // Dropdown should expand
    await expect(page.locator('#oa-team-dropdown')).toBeVisible();
  });

  test('has CSV export button', async ({ page }) => {
    const exportBtn = page.locator('#page-org-analytics button[onclick="oaExportCsv()"]');
    await expect(exportBtn).toBeVisible();
  });
});
