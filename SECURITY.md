# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, use [GitHub's private vulnerability reporting](https://github.com/antkawam/claude-code-aws-gateway/security/advisories/new) to submit your report. You'll receive a response within 48 hours.

### What to include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

## Supported Versions

| Version | Supported |
|---|---|
| 1.x | Yes |
| < 1.0 | No |

## Scope

The following are in scope:

- Authentication bypass (virtual keys, OIDC, session tokens)
- Authorization issues (IDOR, privilege escalation)
- Injection (SQL, command, XSS)
- Credential exposure (tokens, keys, passwords in logs or responses)
- Server-side request forgery (SSRF)
- Denial of service (resource exhaustion, crash bugs)

The following are out of scope:

- Vulnerabilities in dependencies (report upstream, but let us know)
- Issues requiring physical access to the server
- Social engineering
- Rate limiting / brute force on non-auth endpoints
