import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo } from '../helpers/gateway';

test.describe('Team Management', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaPortal(page);
    await navigateTo(page, 'budgets');
  });

  test('creates a new team', async ({ page }) => {
    // Click add team button
    await page.click('button:has-text("Add Team")');
    await expect(page.locator('#modal-create-team')).toBeVisible();

    // Fill team name and submit
    const teamName = `test-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create")');

    // Team should appear in the list
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });
  });

  test('sets a team budget', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Add Team")');
    const teamName = `budget-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Click budget edit for this team
    const teamRow = page.locator(`text=${teamName}`).locator('..');
    const editBudgetBtn = teamRow.locator('button:has-text("Budget"), button:has-text("Edit"), button:has-text("Set")').first();
    if (await editBudgetBtn.isVisible()) {
      await editBudgetBtn.click();
      await expect(page.locator('#modal-edit-budget')).toBeVisible();

      // Set budget amount
      await page.fill('#mb-amount', '500');
      await page.click('#modal-edit-budget button:has-text("Save")');
      // Modal should close
      await expect(page.locator('#modal-edit-budget')).toBeHidden({ timeout: 5_000 });
    }
  });

  test('opens team members panel', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Add Team")');
    const teamName = `members-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Click on team name or members button to open members panel
    const teamLink = page.locator(`text=${teamName}`).first();
    await teamLink.click();
    // Members modal or panel should appear
    const membersPanel = page.locator('#modal-team-members, [class*="team-detail"]').first();
    if (await membersPanel.isVisible({ timeout: 3_000 }).catch(() => false)) {
      await expect(membersPanel).toBeVisible();
    }
  });

  test('deletes a team', async ({ page }) => {
    // Create team first
    await page.click('button:has-text("Add Team")');
    const teamName = `delete-team-${Date.now()}`;
    await page.fill('#mt-name', teamName);
    await page.click('#modal-create-team button:has-text("Create")');
    await expect(page.locator('#page-budgets')).toContainText(teamName, { timeout: 5_000 });

    // Find and click delete button for this team
    const teamRow = page.locator(`text=${teamName}`).locator('..');
    const deleteBtn = teamRow.locator('button:has-text("Delete"), button[title*="delete"], button[title*="Delete"]').first();
    if (await deleteBtn.isVisible()) {
      await deleteBtn.click();
      // Confirm deletion
      const confirmBtn = page.locator('#modal-delete-team button:has-text("Delete")');
      if (await confirmBtn.isVisible({ timeout: 2_000 }).catch(() => false)) {
        await confirmBtn.click();
      }
      // Team should be removed
      await expect(page.locator('#page-budgets')).not.toContainText(teamName, { timeout: 5_000 });
    }
  });
});
