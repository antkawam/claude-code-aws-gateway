# Upgrading

This guide covers how to upgrade CCAG to a new version, handle database migrations, and roll back if needed.

## Checking for Updates

### Current Version

Check the currently deployed version:

```bash
# The image tag is the 8-character commit SHA
# Check what's running in ECS via the AWS console or:
aws ecs describe-services --cluster CCAG --services CCAG \
  --query 'services[0].taskDefinition' --output text
```

### Available Updates

Check for new releases on GitHub:

```bash
git fetch origin
git log HEAD..origin/main --oneline
```

## Docker Compose

Pre-built images are published to GitHub Container Registry on every release.

### Update to latest

```bash
docker compose pull        # Pull latest image from GHCR
docker compose up -d       # Restart with new image
```

### Pin to a specific version

Set `CCAG_VERSION` in your `.env` file or pass it directly:

```bash
CCAG_VERSION=1.1.0 docker compose up -d
```

### Check available versions

```bash
gh release list --repo antkawam/claude-code-aws-gateway
```

## CDK (ECS Fargate)

CDK deployments pull from GHCR by default. Deploy a specific release version:

```bash
cd infra
npx cdk deploy -c environment=prod -c imageTag=1.1.0
```

To review what changed between versions, check the [release notes](https://github.com/antkawam/claude-code-aws-gateway/releases).

## Database Migrations

CCAG uses [sqlx migrations](https://docs.rs/sqlx/latest/sqlx/macro.migrate.html) stored in the `migrations/` directory. Migrations are **applied automatically on startup**. No manual intervention is needed.

### How It Works

1. On startup, the gateway connects to Postgres
2. It checks the `_sqlx_migrations` table for already-applied migrations
3. Any new migrations are applied in order
4. The gateway continues startup after all migrations complete

### Migration Safety

- Migrations are append-only. Existing migration files must not be modified.
- Each migration runs in a transaction
- If a migration fails, the gateway exits with an error and the failed migration is not marked as applied
- ECS will restart the task, retrying the migration

### Checking Migration Status

Connect to the database and query:

```sql
SELECT version, description, installed_on, success
FROM _sqlx_migrations
ORDER BY version;
```

## Breaking Changes Policy

CCAG follows these principles for breaking changes:

- **API compatibility:** The Anthropic Messages API translation is always backward compatible. New Anthropic API features are added; existing behavior is preserved.
- **Admin API:** Changes to admin endpoints are noted in commit messages. The admin API is not yet versioned.
- **Configuration:** New environment variables use sensible defaults. Existing variables retain their behavior.
- **Database schema:** Migrations handle schema changes automatically. No manual SQL is required.

When a breaking change is unavoidable, it will be documented in the release notes with migration instructions.

## Rollback Procedure

If an upgrade causes issues, you can roll back to a previous version.

### Option 1: Redeploy a Previous Image

The quickest rollback. Deploy a known-good image:

```bash
# Find the previous image tag (8-char commit SHA)
git log --oneline -5

# Deploy the previous image directly via CDK
cd infra
npx cdk deploy -c environment=prod -c imageTag=abcd1234
```

### Option 2: Fast Rollback (Skip CloudFormation)

For immediate rollback without CloudFormation:

```bash
# Update the ECS service task definition directly (bypasses CDK)
# See infra/README.md for details on direct ECS task definition updates
```

This directly updates the ECS service task definition, bypassing CDK.

### Option 3: Git Revert and Redeploy

For a permanent rollback:

```bash
git revert HEAD
# Rebuild and redeploy (see infra/README.md for full deployment steps)
```

### Database Rollback Considerations

CCAG migrations are forward-only. If a migration introduced schema changes:

- The previous application version must still be compatible with the new schema (migrations are designed to be backward-compatible where possible)
- If the schema is incompatible, you must manually reverse the migration in the database before rolling back the application

## See Also

- [Getting Started](getting-started.md). Initial deployment.
- [Configuration](configuration.md). Configuration reference.
- [FAQ](faq.md). Troubleshooting.
- See [CHANGELOG.md](../CHANGELOG.md) for release history.
