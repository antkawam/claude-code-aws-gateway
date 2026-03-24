import { Page } from '@playwright/test';

export const ADMIN_USER = process.env.ADMIN_USERNAME || 'admin';
export const ADMIN_PASS = process.env.ADMIN_PASSWORD || 'admin';
export const BASE_URL = process.env.GATEWAY_URL || 'http://localhost:8080';

/** Login via the portal UI and wait for the app shell to appear. */
export async function loginViaPortal(page: Page): Promise<void> {
  await page.goto('/portal');
  await page.waitForSelector('#auth-screen', { state: 'visible' });
  await page.fill('#auth-username', ADMIN_USER);
  await page.fill('#auth-password', ADMIN_PASS);
  await page.click('button:has-text("Sign in")');
  await page.waitForSelector('#app-shell', { state: 'visible', timeout: 10_000 });
}

/** Navigate to a specific portal page via the sidebar. */
export async function navigateTo(page: Page, pageName: string): Promise<void> {
  await page.click(`.nav-item[data-page="${pageName}"]`);
  await page.waitForSelector(`#page-${pageName}.active`, { timeout: 5_000 });
}

/** Make an API call with auth. */
export async function apiCall(
  token: string,
  method: string,
  path: string,
  body?: object,
): Promise<any> {
  const opts: RequestInit = {
    method,
    headers: {
      'content-type': 'application/json',
      'x-api-key': token,
    },
  };
  if (body) opts.body = JSON.stringify(body);
  const resp = await fetch(`${BASE_URL}${path}`, opts);
  return resp.json();
}

/** Get a session token via API (for setup operations). */
export async function getSessionToken(): Promise<string> {
  const resp = await fetch(`${BASE_URL}/auth/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username: ADMIN_USER, password: ADMIN_PASS }),
  });
  const data = await resp.json();
  return data.token;
}
