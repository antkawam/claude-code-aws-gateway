// k6 smoke test: 5 VUs, 60s
// Validates basic gateway functionality under light load.
// Runs in CI on every PR.
import http from 'k6/http';
import { check, sleep } from 'k6';
import { BASE_URL, getSessionToken, authHeaders, streamingPayload } from './helpers.js';

export const options = {
  vus: 5,
  duration: '60s',
  thresholds: {
    http_req_failed: ['rate<0.01'],          // <1% errors
    http_req_duration: ['p(95)<8000'],       // p95 < 8s (realistic stream)
  },
};

export function setup() {
  // Health check
  const health = http.get(`${BASE_URL}/health`);
  check(health, { 'health ok': (r) => r.status === 200 });

  const token = getSessionToken();
  if (!token) throw new Error('Failed to get session token');
  return { token };
}

export default function (data) {
  const resp = http.post(
    `${BASE_URL}/v1/messages`,
    streamingPayload(),
    {
      headers: authHeaders(data.token),
      timeout: '30s',
    },
  );

  check(resp, {
    'status 200': (r) => r.status === 200,
    'has SSE events': (r) => r.body && r.body.includes('event:'),
  });

  // CC user think time: 5-15s between requests
  sleep(Math.random() * 5 + 3);
}
