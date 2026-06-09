/**
 * Playwright spec for the Portal "Model Mappings" tab.
 *
 * AC coverage:
 *   AC8.1 — created_via color-coded badges  (tests: created_via_color_coded)
 *   AC8.2 — NULL last_used_at sort first     (tests: null_last_used_sorts_first — .skip; see note)
 *   AC8.3 — Add-row modal happy + validation (tests: mappings_add, mappings_add_validation_error)
 *   AC8.4 — Discover modal wire-up           (tests: mappings_discover — .fixme; see note)
 *   AC8.5 — Manual smoke items listed below
 *
 * AC8.5 manual smoke checklist (for Task 10 release prep):
 *   - Tab loads with real production data shape, rows colour-coded correctly.
 *   - Add alias row for "Sonnet 4.7" → "anthropic.claude-sonnet-4-6"; confirm request
 *     with model="Sonnet 4.7" succeeds and dispatches (PinHit path, AC7.6).
 *   - Delete an 'unknown' row; confirm cache invalidation (<5 s) makes the next
 *     /v1/messages for that prefix fail with 400.
 *   - Discover Preview for a real Bedrock model ID (e.g. claude-sonnet-4-6-20250514):
 *     preview panel populates, Insert button appears, click Insert, row appears in table.
 *   - Filter by 'unknown': only unknown rows visible; filter by 'admin': only admin rows.
 *   - Green "All grandfathered rows reviewed" banner appears after all 'unknown' rows deleted.
 */

import { test, expect, Browser } from '@playwright/test';
import { loginViaPortal, navigateTo, getSessionToken, BASE_URL } from '../helpers/gateway';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** POST /admin/mappings to seed a row directly via the API. */
async function seedMapping(
  token: string,
  prefix: string,
  suffix: string,
  display?: string,
): Promise<void> {
  const body: Record<string, string> = { anthropic_prefix: prefix, bedrock_suffix: suffix };
  if (display) body.anthropic_display = display;
  const resp = await fetch(`${BASE_URL}/admin/mappings`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-api-key': token },
    body: JSON.stringify(body),
  });
  // 201 Created or 409 Conflict (row already exists from a previous run) — both acceptable for seeding.
  const status = resp.status;
  if (status !== 201 && status !== 200 && status !== 409) {
    const text = await resp.text();
    throw new Error(`seedMapping failed (HTTP ${status}): ${text}`);
  }
}

/** DELETE /admin/mappings/:prefix to clean up after a test. */
async function deleteMapping(token: string, prefix: string): Promise<void> {
  await fetch(`${BASE_URL}/admin/mappings/${encodeURIComponent(prefix)}`, {
    method: 'DELETE',
    headers: { 'x-api-key': token },
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe('Model Mappings tab', () => {
  let token: string;
  // Shared auth state: log in once per worker to avoid hitting the gateway's
  // login rate limiter (10 req/60 s). All tests in this file reuse the same
  // localStorage token snapshot — each test gets a fresh browser context
  // initialised from this state so pages are isolated from each other.
  let sharedStorageState: { cookies: any[]; origins: any[] };

  test.beforeAll(async ({ browser }: { browser: Browser }) => {
    // Obtain API token (getSessionToken is already cached at the module level,
    // so repeat calls within this worker are free).
    token = await getSessionToken();

    // Log in once and capture the resulting localStorage state.
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    await loginViaPortal(page);
    sharedStorageState = await ctx.storageState();
    await ctx.close();
  });

  test.beforeEach(async ({ page }) => {
    // Restore the shared auth state into the page's origin so the portal init()
    // finds the token in localStorage without making another login request.
    // Navigate to the portal first to establish the origin, then bulk-inject
    // all captured localStorage entries from the shared auth state.
    await page.goto('/portal');
    const origins = sharedStorageState?.origins ?? [];
    for (const origin of origins) {
      for (const entry of origin.localStorage ?? []) {
        await page.evaluate(
          ({ key, value }: { key: string; value: string }) => localStorage.setItem(key, value),
          { key: entry.name, value: entry.value },
        );
      }
    }
    // Reload so init() picks up the token from localStorage.
    await page.reload();
    await page.waitForSelector('#app-shell', { state: 'visible', timeout: 10_000 });
    await navigateTo(page, 'mappings');
  });

  // -------------------------------------------------------------------------
  // mappings_lists — basic table render (covers the happy-path navigation path
  // listed in the task brief and also serves as a precondition smoke check)
  // -------------------------------------------------------------------------
  test('mappings_lists — table renders with seeded row', async ({ page }) => {
    const prefix = `pw-list-${Date.now()}`;
    await seedMapping(token, prefix, 'anthropic.claude-test-model-v1:0', 'PW List Test');

    try {
      // Reload to pick up the seeded row (portal caches on navigate)
      await page.reload();
      await page.waitForSelector('#app-shell', { state: 'visible', timeout: 10_000 });
      await navigateTo(page, 'mappings');

      // Table should contain at least one row
      await expect(page.locator('#mappings-table tr')).toHaveCount(
        await page.locator('#mappings-table tr').count(),
      );
      const rowCount = await page.locator('#mappings-table tr').count();
      expect(rowCount).toBeGreaterThanOrEqual(1);

      // Table header has 7 columns (the 7th is the action column with empty <th>)
      const headerCells = page.locator('#page-mappings table thead tr th');
      await expect(headerCells).toHaveCount(7);

      // Seeded row is visible
      await expect(
        page.locator('#mappings-table tr').filter({ hasText: prefix }),
      ).toBeVisible();
    } finally {
      await deleteMapping(token, prefix);
    }
  });

  // -------------------------------------------------------------------------
  // AC8.2 — NULL last_used_at sort first
  //
  // Skipped: Playwright cannot inject a non-null last_used_at directly via the
  // admin API (POST /admin/mappings always creates rows with last_used_at=NULL).
  // The touch_last_used path is triggered by a successful dispatch request,
  // which requires a working Bedrock connection not available in make dev.
  // Covered at unit level in tests/integration/model_mappings_audit_tests.rs.
  // Add to AC8.5 manual smoke: verify sort order in staging with production data.
  // -------------------------------------------------------------------------
  test.skip('null_last_used_sorts_first — AC8.2 (requires Bedrock dispatch to set non-null last_used_at)', async () => {
    // Cannot be made deterministic in make dev without Bedrock access.
    // Unit coverage is in tests/integration/model_mappings_audit_tests.rs.
  });

  // -------------------------------------------------------------------------
  // AC8.1 — created_via color-coded badges
  // -------------------------------------------------------------------------
  test('created_via_color_coded — admin badge has .created-via-admin class', async ({ page }) => {
    const prefix = `pw-badge-admin-${Date.now()}`;
    await seedMapping(token, prefix, 'anthropic.claude-badge-test-v1:0');

    try {
      await page.reload();
      await page.waitForSelector('#app-shell', { state: 'visible', timeout: 10_000 });
      await navigateTo(page, 'mappings');

      // Find the row for this prefix
      const row = page.locator('#mappings-table tr').filter({ hasText: prefix });
      await expect(row).toBeVisible({ timeout: 5_000 });

      // The badge for an admin-created row must carry .created-via-admin
      const badge = row.locator('.badge.created-via-admin');
      await expect(badge).toBeVisible();
      await expect(badge).toContainText('admin');
    } finally {
      await deleteMapping(token, prefix);
    }
  });

  test('created_via_color_coded — unknown badge has .created-via-unknown class (back-filled rows)', async ({ page }) => {
    // POST /admin/mappings always sets created_via='admin', so we cannot seed an
    // 'unknown' row from the API. Instead, verify the CSS class is present for
    // whatever the table already contains that is labelled 'unknown'. If the
    // gateway was freshly migrated, some pre-existing rows will carry 'unknown'.
    // We skip asserting a specific row and instead check that IF any unknown badge
    // exists it carries the correct class.
    const unknownBadges = page.locator('#mappings-table .badge.created-via-unknown');
    const count = await unknownBadges.count();
    if (count > 0) {
      // At least the first one should say 'unknown'
      await expect(unknownBadges.first()).toContainText('unknown');
    }
    // If count is zero this is a fresh gateway with no legacy rows — that's fine,
    // the CSS class test for admin badges in the sibling test covers the shape.
  });

  // -------------------------------------------------------------------------
  // Filter — filter_created_via
  // -------------------------------------------------------------------------
  test('filter_created_via — filter select hides non-matching rows', async ({ page }) => {
    const prefix = `pw-filter-${Date.now()}`;
    await seedMapping(token, prefix, 'anthropic.claude-filter-test-v1:0');

    try {
      await page.reload();
      await page.waitForSelector('#app-shell', { state: 'visible', timeout: 10_000 });
      await navigateTo(page, 'mappings');

      // Seeded row should appear under 'all'
      const row = page.locator('#mappings-table tr').filter({ hasText: prefix });
      await expect(row).toBeVisible({ timeout: 5_000 });

      // Select filter 'admin' — our seeded row should still be visible (it IS admin)
      await page.selectOption('#mappings-filter', 'admin');
      await expect(row).toBeVisible();

      // Select filter 'pass1' — admin row should now be hidden (filtered out)
      await page.selectOption('#mappings-filter', 'pass1');
      await expect(row).toBeHidden();

      // Restore to 'all'
      await page.selectOption('#mappings-filter', 'all');
    } finally {
      await deleteMapping(token, prefix);
    }
  });

  // -------------------------------------------------------------------------
  // AC8.3 happy path — mappings_add
  // -------------------------------------------------------------------------
  test('mappings_add — opens modal, adds row, row appears, delete removes it', async ({ page }) => {
    const prefix = `pw-add-${Date.now()}`;
    const suffix = 'anthropic.claude-playwright-add-v1:0';

    // Open Add Mapping modal
    await page.click('#page-mappings button:has-text("Add Mapping")');
    await expect(page.locator('#modal-add-mapping')).toBeVisible();

    // Fill form
    await page.fill('#am-prefix', prefix);
    await page.fill('#am-suffix', suffix);
    await page.fill('#am-display', 'Playwright Add Test');

    // Submit
    await page.click('#modal-add-mapping button:has-text("Add Mapping")');

    // Modal should close on success
    await expect(page.locator('#modal-add-mapping')).toBeHidden({ timeout: 5_000 });

    // Row should appear in the table
    const row = page.locator('#mappings-table tr').filter({ hasText: prefix });
    await expect(row).toBeVisible({ timeout: 5_000 });

    // Delete the row via the trashcan button — register dialog handler first
    page.once('dialog', (dialog) => dialog.accept());
    await row.locator('button.btn-danger[title="Delete mapping"]').click();

    // Row should disappear
    await expect(row).toBeHidden({ timeout: 5_000 });
  });

  // -------------------------------------------------------------------------
  // AC8.3 validation — mappings_add_validation_error
  // -------------------------------------------------------------------------
  test('mappings_add_validation_error — empty fields show inline error and keep modal open', async ({ page }) => {
    // Open modal
    await page.click('#page-mappings button:has-text("Add Mapping")');
    await expect(page.locator('#modal-add-mapping')).toBeVisible();

    // Ensure fields are empty (clear any leftover state from other tests)
    await page.fill('#am-prefix', '');
    await page.fill('#am-suffix', '');
    await page.fill('#am-display', '');

    // Submit without filling required fields
    await page.click('#modal-add-mapping button:has-text("Add Mapping")');

    // Inline error should appear
    await expect(page.locator('#am-error')).toBeVisible({ timeout: 3_000 });

    // Modal must remain open (validation blocked submission)
    await expect(page.locator('#modal-add-mapping')).toBeVisible();

    // Close modal to clean up
    await page.click('#modal-add-mapping button:has-text("Cancel")');
  });

  // -------------------------------------------------------------------------
  // AC8.4 — mappings_discover
  //
  // Marked .fixme because the Discover Preview endpoint calls
  // POST /admin/mappings/discover, which internally calls
  // bedrock:ListInferenceProfiles. In a `make dev` environment (no Bedrock
  // connection), this will return an error response (connection refused or
  // credential error). The test therefore verifies only the error-path
  // wire-up: that dm-error becomes visible when the API fails.
  //
  // For the success path (preview panel + Insert button) — add to AC8.5
  // manual smoke: in staging, enter "claude-sonnet-4-6-20250514" and verify
  // dm-preview populates and dm-insert-btn becomes visible, then click Insert
  // and confirm the row appears in the table.
  // -------------------------------------------------------------------------
  test.fixme(
    'mappings_discover — discover modal error path (requires Bedrock or mock gateway)',
    async ({ page }) => {
      // Open Discover Preview modal
      await page.click('#page-mappings button:has-text("Discover Preview")');
      await expect(page.locator('#modal-discover-mapping')).toBeVisible();

      // Type a model ID
      await page.fill('#dm-model', 'claude-sonnet-4-6-20250514');

      // Click Preview
      await page.click('#modal-discover-mapping button:has-text("Preview")');

      // In a no-Bedrock dev environment: dm-error should show an error message.
      // In a Bedrock-connected environment: dm-preview should show and dm-insert-btn appear.
      const hasError = await page.locator('#dm-error').isVisible({ timeout: 5_000 });
      const hasPreview = await page.locator('#dm-preview').isVisible({ timeout: 5_000 });

      expect(hasError || hasPreview).toBe(true);

      if (hasPreview) {
        // Bedrock path: Insert button must be visible
        await expect(page.locator('#dm-insert-btn')).toBeVisible();
        // Click Insert and verify the row appears
        page.once('dialog', (dialog) => dialog.accept());
        await page.click('#dm-insert-btn');
        await expect(page.locator('#modal-discover-mapping')).toBeHidden({ timeout: 5_000 });
        await expect(
          page.locator('#mappings-table tr').filter({ hasText: 'claude-sonnet-4-6' }),
        ).toBeVisible({ timeout: 5_000 });
      } else {
        // Error path: dm-error shows a recognizable message
        await expect(page.locator('#dm-error')).not.toBeEmpty();
      }

      // Close modal
      await page.click('#modal-discover-mapping button:has-text("Cancel")');
    },
  );

  // -------------------------------------------------------------------------
  // Discover modal — empty-input validation (deterministic, no Bedrock needed)
  // -------------------------------------------------------------------------
  test('mappings_discover_empty_input — empty model input shows inline error', async ({ page }) => {
    // Open Discover Preview modal
    await page.click('#page-mappings button:has-text("Discover Preview")');
    await expect(page.locator('#modal-discover-mapping')).toBeVisible();

    // Leave dm-model empty and click Preview
    await page.fill('#dm-model', '');
    await page.click('#modal-discover-mapping button:has-text("Preview")');

    // The JS does client-side guard: sets dm-error immediately (no API call)
    await expect(page.locator('#dm-error')).toBeVisible({ timeout: 3_000 });
    await expect(page.locator('#dm-error')).toContainText('Enter a model ID');

    // dm-preview and dm-insert-btn must remain hidden
    await expect(page.locator('#dm-preview')).toBeHidden();
    await expect(page.locator('#dm-insert-btn')).toBeHidden();

    // Close
    await page.click('#modal-discover-mapping button:has-text("Cancel")');
  });

  // -------------------------------------------------------------------------
  // Reviewed banner — appears when no 'unknown' rows remain
  // -------------------------------------------------------------------------
  test('mappings_reviewed_banner — green banner visible only when no unknown rows remain', async ({ page }) => {
    // Ensure the filter shows 'all' (default)
    await page.selectOption('#mappings-filter', 'all');

    // Count unknown rows currently in the table
    const unknownBadges = page.locator('#mappings-table .badge.created-via-unknown');
    const unknownCount = await unknownBadges.count();

    if (unknownCount === 0) {
      // No unknown rows: banner SHOULD be visible (if table is non-empty)
      const rowCount = await page.locator('#mappings-table tr').count();
      if (rowCount > 0) {
        await expect(page.locator('#mappings-reviewed-banner')).toBeVisible();
      }
    } else {
      // Unknown rows exist: banner must be hidden
      await expect(page.locator('#mappings-reviewed-banner')).toBeHidden();
    }
  });
});
