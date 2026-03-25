// k6 stress test: ramp 10 → 200 → 300 VUs over 15 minutes
// Identifies breaking point: where latency degrades, errors appear, or OOM.
// Run with realistic stream duration: MOCK_CHUNKS=100 MOCK_TTFT_MS=1500
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Counter, Gauge } from 'k6/metrics';
import { BASE_URL, getSessionToken, authHeaders, streamingPayload } from './helpers.js';

const streamDuration = new Trend('stream_duration_ms');
const requestsCompleted = new Counter('requests_completed');
const requestsFailed = new Counter('requests_failed');

export const options = {
  scenarios: {
    stress_ramp: {
      executor: 'ramping-vus',
      startVUs: 10,
      stages: [
        { duration: '2m', target: 50 },     // warm up
        { duration: '3m', target: 100 },    // moderate load
        { duration: '3m', target: 200 },    // heavy load
        { duration: '3m', target: 300 },    // stress / find breaking point
        { duration: '2m', target: 100 },    // recovery
        { duration: '2m', target: 0 },      // cool down
      ],
    },
  },
  thresholds: {
    // These are intentionally lenient - stress test is about finding limits
    http_req_failed: [{ threshold: 'rate<0.20', abortOnFail: true }],  // abort if >20% errors
    http_req_duration: ['p(95)<30000'],  // p95 < 30s
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
    { headers: authHeaders(data.token), timeout: '60s' },
  );
  const duration = Date.now() - start;
  streamDuration.add(duration);

  const ok = check(resp, {
    'status 200': (r) => r.status === 200,
    'has events': (r) => r.body && r.body.includes('event:'),
  });

  if (ok) {
    requestsCompleted.add(1);
  } else {
    requestsFailed.add(1);
  }

  // Shorter think time for stress: 2-5s
  sleep(Math.random() * 3 + 2);
}

export function handleSummary(data) {
  const p50 = data.metrics.http_req_duration?.values?.['p(50)'] || 0;
  const p95 = data.metrics.http_req_duration?.values?.['p(95)'] || 0;
  const p99 = data.metrics.http_req_duration?.values?.['p(99)'] || 0;
  const errRate = data.metrics.http_req_failed?.values?.rate || 0;
  const totalReqs = data.metrics.requests_completed?.values?.count || 0;
  const failedReqs = data.metrics.requests_failed?.values?.count || 0;
  const avgStream = data.metrics.stream_duration_ms?.values?.avg || 0;
  const maxVUs = data.metrics.vus_max?.values?.value || 0;

  // Estimate max sustainable users based on error rate inflection
  let maxSustainable = maxVUs;
  if (errRate > 0.05) maxSustainable = Math.floor(maxVUs * 0.7);
  if (errRate > 0.10) maxSustainable = Math.floor(maxVUs * 0.5);

  const report = `
=== Stress Test Scalability Assessment ===
Peak VUs reached:         ${maxVUs}
Total requests:           ${totalReqs + failedReqs}
Successful:               ${totalReqs}
Failed:                   ${failedReqs}
Error rate:               ${(errRate * 100).toFixed(2)}%
Avg stream duration:      ${(avgStream / 1000).toFixed(1)}s
p50 request duration:     ${(p50 / 1000).toFixed(1)}s
p95 request duration:     ${(p95 / 1000).toFixed(1)}s
p99 request duration:     ${(p99 / 1000).toFixed(1)}s

--- Scalability Estimate ---
Max sustained users before errors:  ~${maxSustainable}
Recommendation:  ${maxSustainable > 200
    ? 'Single container handles 200+ concurrent users. Scale at 150+ for headroom.'
    : maxSustainable > 100
    ? 'Scale ECS at ~' + Math.floor(maxSustainable * 0.7) + ' concurrent users.'
    : 'Container saturates early. Check DB pool size, Tokio runtime, and memory.'}
`;

  return {
    stdout: report,
    'tests/load/results/stress-summary.json': JSON.stringify(data, null, 2),
  };
}
