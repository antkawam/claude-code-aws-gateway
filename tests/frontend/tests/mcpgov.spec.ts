import { test, expect } from '@playwright/test';
import { loginViaPortal, navigateTo, getSessionToken, apiCall } from '../helpers/gateway';

// MCP / tool governance. The page is injected at runtime by
// /portal/ext/mcp-governance.js rather than living in static/index.html, so these
// tests double as a check that the self-injection works in a real browser.
test.describe('MCP Governance', () => {
  test.beforeEach(async ({ page }) => {
    await loginViaPortal(page);
  });

  test('injects its nav item into the admin section', async ({ page }) => {
    const navItem = page.locator('#nav-admin-section .nav-item[data-page="mcpgov"]');
    await expect(navItem).toBeVisible();
    await expect(navItem).toContainText('MCP & Tools');
  });

  test('opens the page with overview stats', async ({ page }) => {
    await navigateTo(page, 'mcpgov');

    await expect(page.locator('#page-mcpgov')).toContainText('MCP & Tools Governance');
    // Overview tab renders the posture card once the summary API answers.
    await expect(page.locator('#mcpgov-body')).toContainText('Current posture', {
      timeout: 5_000,
    });
    await expect(page.locator('#mcpgov-body')).toContainText('MCP servers');
  });

  test('renders the catalog and can approve a server', async ({ page }) => {
    // Seed the catalog through the admin API rather than by sending a /v1/messages
    // request. Discovery-from-traffic is covered server-side; driving it here would
    // make this test depend on reachable Bedrock models, which the frontend suite
    // (built without the mock-bedrock feature) does not have.
    const token = await getSessionToken();
    const server = `fetest${Date.now()}`;
    await apiCall(token, 'POST', '/admin/mcpgov/servers', {
      server_name: server,
      status: 'pending',
    });

    await navigateTo(page, 'mcpgov');
    await page.click('#mcpgov-tabs button[data-tab="catalog"]');

    const card = page.locator('#mcpgov-body .card').filter({ hasText: server });
    await expect(card).toBeVisible({ timeout: 5_000 });
    await expect(card).toContainText('pending');

    await card.locator('button:has-text("Approve")').first().click();
    await expect(page.locator('#toast-container')).toContainText('approved', {
      timeout: 5_000,
    });

    // The re-rendered card reflects the new status.
    await expect(
      page.locator('#mcpgov-body .card').filter({ hasText: server }),
    ).toContainText('approved', { timeout: 5_000 });
  });

  test('creates and deletes a team policy via the modal', async ({ page }) => {
    // A team to scope the policy to (user-scope would work too, but team exercises
    // the UUID validation path).
    const token = await getSessionToken();
    const team = await apiCall(token, 'POST', '/admin/teams', {
      name: `mcpgov-fe-team-${Date.now()}`,
    });

    await navigateTo(page, 'mcpgov');
    await page.click('button:has-text("New Policy")');
    await expect(page.locator('#modal-mcpgov-policy')).toBeVisible();

    await page.selectOption('#mcpgov-p-scope', 'team');
    await page.fill('#mcpgov-p-ref', team.id);
    await page.selectOption('#mcpgov-p-mode', 'warn');
    await page.fill('#mcpgov-p-deny', 'mcp__fe_denied__*');
    await page.click('#modal-mcpgov-policy button:has-text("Save")');

    await expect(page.locator('#toast-container')).toContainText('Policy saved', {
      timeout: 5_000,
    });

    // The policies tab lists it.
    await page.click('#mcpgov-tabs button[data-tab="policies"]');
    await expect(page.locator('#mcpgov-body')).toContainText(`team: ${team.id}`, {
      timeout: 5_000,
    });

    // And it can be deleted (the global policy shows no Delete button; team rows do).
    page.on('dialog', (d) => d.accept());
    const row = page.locator('#mcpgov-body tr').filter({ hasText: team.id });
    await row.locator('button:has-text("Delete")').click();
    await expect(page.locator('#toast-container')).toContainText('Policy deleted', {
      timeout: 5_000,
    });
  });

  // Regression: MCP server and tool names come from client-supplied tool definitions,
  // so they are untrusted. An earlier version interpolated them into inline
  // onclick="fn('name')" handlers using the SPA's esc(), which does not escape quotes
  // (textContent -> innerHTML leaves them alone). A name containing a double quote
  // terminated the attribute early, breaking the handler and opening a markup
  // injection path. Row actions now use data attributes plus delegated dispatch.
  test('handles server names containing quotes and markup safely', async ({ page }) => {
    const token = await getSessionToken();
    const nasty = `q"ote'<img src=x onerror=window.__pwned=1>${Date.now()}`;
    await apiCall(token, 'POST', '/admin/mcpgov/servers', {
      server_name: nasty,
      status: 'pending',
    });

    await navigateTo(page, 'mcpgov');
    await page.click('#mcpgov-tabs button[data-tab="catalog"]');

    const card = page.locator('#mcpgov-body .card').filter({ hasText: nasty });
    await expect(card).toBeVisible({ timeout: 5_000 });

    // The name is rendered as text, not parsed as markup.
    expect(await page.locator('#mcpgov-body img').count()).toBe(0);

    // The action still works despite the quotes, and no injected script ran.
    await card.locator('button[data-act="server-status"][data-status="denied"]').click();
    await expect(page.locator('#toast-container')).toContainText('denied', {
      timeout: 5_000,
    });
    expect(await page.evaluate(() => (window as any).__pwned)).toBeUndefined();
  });

  test('simulator returns verdicts for an arbitrary tool', async ({ page }) => {
    await navigateTo(page, 'mcpgov');
    await page.click('#mcpgov-tabs button[data-tab="simulate"]');

    await page.fill('#mcpgov-sim-tools', 'mcp__simtest__some_tool');
    await page.click('button:has-text("Simulate")');

    await expect(page.locator('#mcpgov-body')).toContainText('Effective policy', {
      timeout: 5_000,
    });
    await expect(page.locator('#mcpgov-body')).toContainText('mcp__simtest__some_tool');
  });
});
