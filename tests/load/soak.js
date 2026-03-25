// k6 soak test: 30 VUs for 30 minutes at steady state
// Detects: memory leaks, connection pool exhaustion, p99 drift over time
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Counter } from 'k6/metrics';
import { BASE_URL, getSessionToken, authHeaders, streamingPayload } from './helpers.js';

const streamDuration = new Trend('stream_duration_ms');
const requestsCompleted = new Counter('requests_completed');

export const options = {
  scenarios: {
    soak: {
      executor: 'ramping-vus',
      startVUs: 5,
      stages: [
        { duration: '1m', target: 30 },    // ramp up
        { duration: '28m', target: 30 },   // hold steady
        { duration: '1m', target: 0 },     // ramp down
      ],
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.02'],          // <2% errors over 30min
    http_req_duration: ['p(99)<15000'],      // p99 stays under 15s
  },
};

export function setup() {
  const token = getSessionToken();
  if (!token) throw new Error('Failed to get session token');
  return { token };
}

export default function (data) {
  const start = Date.now();
  const resp = http.post(
    `${BASE_URL}/v1/messages`,
    streamingPayload(),
    { headers: authHeaders(data.token), timeout: '30s' },
  );
  streamDuration.add(Date.now() - start);

  check(resp, {
    'status 200': (r) => r.status === 200,
    'has events': (r) => r.body && r.body.includes('event:'),
  });
  requestsCompleted.add(1);

  // CC user think time: 5-15s
  sleep(Math.random() * 10 + 5);
}

export function handleSummary(data) {
  const p95 = data.metrics.http_req_duration?.values?.['p(95)'] || 0;
  const p99 = data.metrics.http_req_duration?.values?.['p(99)'] || 0;
  const errRate = data.metrics.http_req_failed?.values?.rate || 0;
  const totalReqs = data.metrics.requests_completed?.values?.count || 0;
  const avgStream = data.metrics.stream_duration_ms?.values?.avg || 0;

  const report = `
=== Soak Test Assessment (30 min, 30 VUs) ===
Total requests:           ${totalReqs}
Error rate:               ${(errRate * 100).toFixed(2)}%
Avg stream duration:      ${(avgStream / 1000).toFixed(1)}s
p95 duration:             ${(p95 / 1000).toFixed(1)}s
p99 duration:             ${(p99 / 1000).toFixed(1)}s

Check for:
- p99 drift: did p99 increase over time? (check JSON for time series)
- Error clustering: did errors appear in bursts? (connection pool exhaustion)
- Memory: check gateway RSS before/after (should be stable)
`;

  return {
    stdout: report,
    'tests/load/results/soak-summary.json': JSON.stringify(data, null, 2),
  };
}
