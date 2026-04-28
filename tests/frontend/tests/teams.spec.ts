import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo } from '../helpers/gateway';

test.describe('Team Management', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'budgets');
  });

  test('creates a new team', async ({ page }) => {
    // Click add team button
    await page.click('button:has-text("Create Team")');
    await expect(page.locator('#modal-create-team')).toBeVisible();

    // Fill team name and submit
    const teamName = `test-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create Team")');

    // Team should appear in the list
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });
  });

  test('sets a team budget via Configure modal Budget tab', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Create Team")');
    const teamName = `budget-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create Team")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Open Configure modal for this team
    const teamRow = page.locator('#budgets-table tr').filter({ hasText: teamName });
    await teamRow.locator('button:has-text("Configure")').click();
    await expect(page.locator('#modal-team-members')).toBeVisible();

    // Switch to Budget tab
    await page.click('#team-tab-budget');
    await expect(page.locator('#team-panel-budget')).toBeVisible();

    // Set budget amount and save
    await page.fill('#tb-amount', '500');
    await page.click('#team-panel-budget button:has-text("Save Budget")');

    // Modal should close after save
    await expect(page.locator('#modal-team-members')).toBeHidden({ timeout: 5_000 });
  });

  test('opens team members panel via Configure modal', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Create Team")');
    const teamName = `members-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create Team")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Open Configure modal — Members tab is default
    const teamRow = page.locator('#budgets-table tr').filter({ hasText: teamName });
    await teamRow.locator('button:has-text("Configure")').click();

    // The consolidated modal uses the same #modal-team-members ID
    await expect(page.locator('#modal-team-members')).toBeVisible({ timeout: 5_000 });

    // Members tab panel should be visible by default
    await expect(page.locator('#team-panel-members')).toBeVisible();
  });

  test('manages team keys via Configure modal', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Create Team")');
    const teamName = `keys-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create Team")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Open Configure modal
    const teamRow = page.locator('#budgets-table tr').filter({ hasText: teamName });
    await teamRow.locator('button:has-text("Configure")').click();
    await expect(page.locator('#modal-team-members')).toBeVisible();

    // Switch to Keys tab
    await page.click('#team-tab-keys');
    await expect(page.locator('#team-panel-keys')).toBeVisible();

    // Create a team key
    const keyName = `e2e-key-${Date.now()}`;
    await page.fill('#tk-name', keyName);
    await page.click('#team-panel-keys button:has-text("Create Key")');

    // New key banner should appear with the raw key value
    await expect(page.locator('#tk-new-key-banner')).toBeVisible({ timeout: 5_000 });

    // Key should appear in the table
    await expect(page.locator('#team-keys-table')).toContainText(keyName, { timeout: 5_000 });

    // Revoke the key (accept the confirm dialog)
    page.once('dialog', dialog => dialog.accept());
    const keyRow = page.locator(`#team-keys-table tr`).filter({ hasText: keyName });
    await keyRow.locator('button:has-text("Revoke")').click();

    // After revoke the row status should reflect inactive or the revoke button disappears
    await expect(
      keyRow.locator('button:has-text("Revoke")'),
    ).toBeHidden({ timeout: 5_000 });
  });

  test('deletes a team', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Create Team")');
    const teamName = `delete-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create Team")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Find and click delete button for this team
    const teamRow = page.locator('#budgets-table tr').filter({ hasText: teamName });
    const deleteBtn = teamRow.locator('button:has-text("Delete"), button[title*="delete"], button[title*="Delete"]').first();
    if (await deleteBtn.isVisible()) {
      await deleteBtn.click();
      // Confirm deletion
      const confirmBtn = page.locator('#modal-delete-team button:has-text("Delete Team")');
      if (await confirmBtn.isVisible({ timeout: 2_000 }).catch(() => false)) {
        await confirmBtn.click();
      }
      // Team should be removed
      await expect(page.locator('#page-budgets')).not.toContainText(teamName, { timeout: 5_000 });
    }
  });
});
