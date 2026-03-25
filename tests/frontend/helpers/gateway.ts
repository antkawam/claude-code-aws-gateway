import { Page } from '@playwright/test';

export const ADMIN_USER = process.env.ADMIN_USERNAME || 'admin';
export const ADMIN_PASS = process.env.ADMIN_PASSWORD || 'admin';
export const BASE_URL = process.env.GATEWAY_URL || 'http://localhost:8080';

/** Get a session token via API. */
export async function getSessionToken(): Promise<string> {
  const resp = await fetch(`${BASE_URL}/auth/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username: ADMIN_USER, password: ADMIN_PASS }),
  });
  const data = await resp.json();
  if (!data.token) throw new Error(`Login failed: ${JSON.stringify(data)}`);
  return data.token;
}

// Cache the token across tests to avoid hitting the rate limit (10 req/60s).
let cachedToken: string | null = null;

/**
 * Login by injecting a session token into localStorage, then navigating to the portal.
 * This avoids the UI login flow and the gateway's login rate limit.
 */
export async function loginViaPortal(page: Page): Promise<void> {
  if (!cachedToken) cachedToken = await getSessionToken();

  // Navigate to portal to establish the origin, then inject the token
  await page.goto('/portal');
  await page.evaluate((token) => {
    localStorage.setItem('proxyApiKey', token);
  }, cachedToken);

  // Reload so the init() function picks up the token from localStorage
  await page.reload();
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
