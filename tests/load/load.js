// k6 load test: 50 concurrent CC users, 5 minutes
// Simulates realistic CC workload: ~10-15 req/hr per user, 5-15s streams
import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Counter } from 'k6/metrics';
import { BASE_URL, getSessionToken, authHeaders, streamingPayload, nonStreamingPayload } from './helpers.js';

const streamDuration = new Trend('stream_duration_ms');
const requestsCompleted = new Counter('requests_completed');

export const options = {
  scenarios: {
    cc_users: {
      executor: 'ramping-vus',
      startVUs: 5,
      stages: [
        { duration: '30s', target: 25 },   // ramp to 25
        { duration: '30s', target: 50 },   // ramp to 50
        { duration: '3m', target: 50 },    // hold at 50
        { duration: '30s', target: 0 },    // ramp down
      ],
    },
  },
  thresholds: {
    http_req_failed: ['rate<0.05'],          // <5% errors
    http_req_duration: ['p(95)<10000'],      // p95 < 10s
    stream_duration_ms: ['p(99)<15000'],     // p99 stream < 15s
  },
};

export function setup() {
  const token = getSessionToken();
  if (!token) throw new Error('Failed to get session token');
  return { token };
}

export default function (data) {
  // 80% streaming, 10% non-streaming, 10% health
  const roll = Math.random();

  if (roll < 0.1) {
    // Health check
    http.get(`${BASE_URL}/health`);
  } else if (roll < 0.2) {
    // Non-streaming request
    const resp = http.post(
      `${BASE_URL}/v1/messages`,
      nonStreamingPayload(),
      { headers: authHeaders(data.token), timeout: '30s' },
    );
    check(resp, { 'non-stream 200': (r) => r.status === 200 });
    requestsCompleted.add(1);
  } else {
    // Streaming request (primary workload)
    const start = Date.now();
    const resp = http.post(
      `${BASE_URL}/v1/messages`,
      streamingPayload(),
      { headers: authHeaders(data.token), timeout: '30s' },
    );
    const duration = Date.now() - start;
    streamDuration.add(duration);

    check(resp, {
      'stream 200': (r) => r.status === 200,
      'has events': (r) => r.body && r.body.includes('event:'),
    });
    requestsCompleted.add(1);
  }

  // CC user think time between requests (5-15s)
  sleep(Math.random() * 10 + 5);
}

export function handleSummary(data) {
  const p95 = data.metrics.http_req_duration?.values?.['p(95)'] || 0;
  const p99 = data.metrics.http_req_duration?.values?.['p(99)'] || 0;
  const errRate = data.metrics.http_req_failed?.values?.rate || 0;
  const reqs = data.metrics.requests_completed?.values?.count || 0;
  const avgStream = data.metrics.stream_duration_ms?.values?.avg || 0;
  const maxVUs = data.metrics.vus_max?.values?.value || 0;

  const report = `
=== Load Test Scalability Assessment (${maxVUs} VUs) ===
Requests completed:     ${reqs}
Error rate:             ${(errRate * 100).toFixed(2)}%
Avg stream duration:    ${(avgStream / 1000).toFixed(1)}s
p95 request duration:   ${(p95 / 1000).toFixed(1)}s
p99 request duration:   ${(p99 / 1000).toFixed(1)}s
Peak concurrent users:  ${maxVUs}
`;

  return {
    stdout: report,
    'tests/load/results/load-summary.json': JSON.stringify(data, null, 2),
  };
}
