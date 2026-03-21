# Contributing to CCAG

Thank you for your interest in contributing to Claude Code AWS Gateway.

## Development Setup

### Prerequisites

- Rust (latest stable, 2024 edition)
- Docker (for Postgres and integration tests)
- AWS CLI v2 (only needed for e2e tests)

### Getting Started

```bash
cd claude-code-aws-gateway

# Start Postgres and build
make dev
# Follow the printed DATABASE_URL command, e.g.:
DATABASE_URL=postgres://proxy:devpass@127.0.0.1:5432/proxy cargo run
```

The gateway starts at `http://127.0.0.1:8080` with default admin credentials (`admin`/`admin`). Open `http://127.0.0.1:8080/portal` for the admin UI.

If port 5432 is already in use, set a different port: `POSTGRES_PORT=5488 make dev`.

### Running Tests

```bash
make test                # Unit tests (fast, no dependencies)
make lint                # Format check + clippy
make test-integration    # Integration tests (spins up Postgres via Docker)
make check               # All of the above (what CI runs)
make test-e2e            # E2e HTTP tests (requires AWS credentials for Bedrock)
```

Run `make help` to see all available commands.

## Project Structure

See the [README](README.md#development) for a full overview. Key areas:

- `src/translate/`: API translation (Anthropic <-> Bedrock)
- `src/api/`: HTTP handlers, admin API, auth endpoints
- `src/auth/`: Virtual keys, OIDC JWT validation, sessions
- `src/db/`: Postgres operations and migrations
- `static/index.html`: Admin portal SPA (embedded at compile time)
- `infra/`: AWS CDK infrastructure (TypeScript)

## Making Changes

### Code Style

```bash
make fmt     # Auto-format
make lint    # Check formatting + clippy
```

CI runs `make lint` on every PR. Ensure it passes before pushing.

### Adding a Database Migration

1. Create `migrations/YYYYMMDD_NNN_description.sql` (sequential numbering)
2. Write idempotent SQL (`IF NOT EXISTS`, `CREATE OR REPLACE`)
3. Migrations run automatically on gateway startup
4. Test: `make test-integration`

### Modifying the Portal

The admin portal (`static/index.html`) is embedded at compile time via `include_str!`. Recompile the gateway after edits.

### Adding New Environment Variables

1. Read the variable in `src/config/mod.rs` or the relevant module
2. Add to the env vars table in `docs/configuration.md`
3. Add to the CDK stack (`infra/stack.ts`) if needed in ECS

## Pull Requests

1. Fork the repository and create a feature branch
2. Make your changes with tests where applicable
3. Run `make check` to verify
4. Open a pull request with a clear description

## Reporting Issues

Open an issue on GitHub with:
- A clear description of the problem or feature request
- Steps to reproduce (for bugs)
- Expected vs actual behavior
- Environment details (OS, Rust version, AWS region)

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
