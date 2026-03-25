// k6 load test helpers: auth, request builders, CC-realistic payloads
import http from 'k6/http';

export const BASE_URL = __ENV.GATEWAY_URL || 'http://localhost:8080';
const ADMIN_USER = __ENV.ADMIN_USERNAME || 'admin';
const ADMIN_PASS = __ENV.ADMIN_PASSWORD || 'admin';

// Cache session token across VUs
let _cachedToken = null;

export function getSessionToken() {
  if (_cachedToken) return _cachedToken;

  const resp = http.post(`${BASE_URL}/auth/login`, JSON.stringify({
    username: ADMIN_USER,
    password: ADMIN_PASS,
  }), { headers: { 'content-type': 'application/json' } });

  if (resp.status !== 200) {
    console.error(`Login failed: ${resp.status} ${resp.body}`);
    return null;
  }

  _cachedToken = JSON.parse(resp.body).token;
  return _cachedToken;
}

export function authHeaders(token) {
  return {
    'content-type': 'application/json',
    'x-api-key': token,
    'anthropic-version': '2023-06-01',
  };
}

// CC-realistic streaming request payload (~4K input tokens via system prompt)
export function streamingPayload() {
  return JSON.stringify({
    model: 'claude-sonnet-4-6-20250514',
    max_tokens: 4096,
    stream: true,
    system: 'You are an expert software engineer helping with a Rust project. ' +
      'The codebase is a self-hosted API gateway that routes requests through Amazon Bedrock. ' +
      'Follow best practices for error handling, testing, and documentation. '.repeat(20),
    messages: [
      {
        role: 'user',
        content: 'Review the following code and suggest improvements for error handling and performance.',
      },
    ],
  });
}

// Non-streaming request payload
export function nonStreamingPayload() {
  return JSON.stringify({
    model: 'claude-sonnet-4-6-20250514',
    max_tokens: 256,
    stream: false,
    messages: [
      { role: 'user', content: 'Say hello in one sentence.' },
    ],
  });
}
