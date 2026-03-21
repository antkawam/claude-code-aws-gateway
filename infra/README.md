# Deploying Claude Code AWS Gateway (CCAG)

Production deployment guide for CCAG, an API gateway that translates the Anthropic Messages API to AWS Bedrock.

## Prerequisites

- **AWS account** with Bedrock model access enabled for Claude models in your target region
- **AWS CLI v2** configured with credentials (`aws sts get-caller-identity` should succeed)
- **Node.js 18+** and npm
- **Docker** (for building the ARM64 container image)
- **AWS CDK CLI**: installed globally (`npm install -g aws-cdk`) or use `npx cdk` from the `infra/` directory

## Architecture

```
                Internet
                   │
            ┌──────▼──────┐
            │     ALB      │
            │  (HTTPS/443) │
            └──────┬───────┘
                   │
          ┌────────▼────────┐
          │  ECS Fargate     │
          │  ARM64 (Graviton)│───── AWS Bedrock
          │  x N tasks       │     (Claude models)
          └────────┬─────────┘
                   │
          ┌────────▼─────────┐
          │  RDS PostgreSQL   │
          │  (db.t4g.small)   │
          └──────────────────┘
```

### AWS Resources Created

The CDK stack provisions the following resources:

| Category | Resources |
|---|---|
| **Networking** | VPC (2 AZs), public subnets, private subnets, NAT gateway (1), internet gateway |
| **Compute** | ECS cluster (Container Insights enabled), Fargate service, task definition (ARM64, 0.5 vCPU, 1 GB) |
| **Load Balancing** | Application Load Balancer (public), target group, HTTPS listener (with cert) or HTTP listener |
| **Database** | RDS PostgreSQL 16 (db.t4g.small, 20 GB gp2, auto-scaling to 100 GB, encrypted, 7-day backups, Performance Insights) |
| **DNS/TLS** | Route53 A record (if domain configured), ACM certificate (auto-created via DNS validation, or bring your own) |
| **Secrets** | Secrets Manager: DB credentials (auto-generated), API key (auto-generated), Admin key (auto-generated) |
| **Autoscaling** | Target tracking on CPU (70%) and memory (80%), min = desiredCount, max = desiredCount * 5 |
| **Monitoring** | CloudWatch log group (1 month retention), 8 CloudWatch alarms, SNS alarm topic, optional webhook forwarding via EventBridge |
| **IAM** | Task role with Bedrock invoke + Service Quotas read, execution role with ECR pull |

### Estimated Monthly Cost

For a minimal deployment (1 task, db.t4g.small, 1 NAT gateway):

- NAT Gateway: ~$35
- ALB: ~$20
- ECS Fargate (0.5 vCPU, 1 GB, ARM64): ~$15
- RDS db.t4g.small: ~$30
- Secrets Manager (3 secrets): ~$1.20
- CloudWatch: ~$5

**Total: ~$107/month** (excluding Bedrock API usage and data transfer)

## Quick Deploy

### 1. Clone the Repository

```bash
cd claude-code-aws-gateway
```

### 2. Install CDK Dependencies

```bash
cd infra
npm install
```

### 3. Create `environments.json`

Create `environments.json` in the **project root** (not in `infra/`). This is the single source of truth for deployment configuration.

```json
{
  "region": "us-west-2",
  "ecr_repo_name": "ccag",
  "stack_name": "CCAG",
  "prod": {
    "account_id": "123456789012",
    "domain_name": "ccag.example.com",
    "hosted_zone_name": "example.com",
    "certificate_arn": null,
    "admin_users": "user@example.com",
    "desired_count": 2
  }
}
```

See [Configuration](#configuration) below for all available fields.

### 4. Create the ECR Repository

```bash
aws ecr create-repository --repository-name ccag --region us-west-2
```

### 5. Build and Push the Docker Image

> **Note:** Steps 5-9 run from the project root. If you followed step 2 (`cd infra`), run `cd ..` first.

```bash
# Authenticate Docker with ECR
aws ecr get-login-password --region us-west-2 | \
  docker login --username AWS --password-stdin 123456789012.dkr.ecr.us-west-2.amazonaws.com

# Build for ARM64 (required: ECS tasks run on Graviton)
docker buildx build --platform linux/arm64 -t ccag .

# Tag and push
IMAGE_TAG=$(git rev-parse --short=8 HEAD)
docker tag ccag:latest 123456789012.dkr.ecr.us-west-2.amazonaws.com/ccag:${IMAGE_TAG}
docker push 123456789012.dkr.ecr.us-west-2.amazonaws.com/ccag:${IMAGE_TAG}
```

### 6. Bootstrap CDK (First Time Only)

```bash
cd infra
npx cdk bootstrap aws://123456789012/us-west-2 -c environment=prod
```

Replace `prod` with your environment name from `environments.json` if different.

### 7. Deploy

```bash
cd infra
npx cdk deploy -c environment=prod -c imageTag=${IMAGE_TAG}
```

CDK will show the resources it plans to create. Review and confirm.

### 8. Access the Gateway

After deployment, CDK prints several outputs:

- **GatewayUrl**: the base URL for API requests
- **PortalUrl**: the self-service admin portal
- **ApiKeySecretArn**: retrieve the auto-generated API key:
  ```bash
  aws secretsmanager get-secret-value --secret-id <ApiKeySecretArn> --query SecretString --output text
  ```
- **AdminKeySecretArn**: retrieve the admin API key
- **AlarmTopicArn**: subscribe to alarm notifications:
  ```bash
  aws sns subscribe --topic-arn <AlarmTopicArn> --protocol email --notification-endpoint you@example.com
  ```

### 9. Configure Claude Code

Point Claude Code at your gateway:

```bash
export ANTHROPIC_BASE_URL=https://ccag.example.com
export ANTHROPIC_API_KEY=<api-key-from-step-8>
```

## Configuration

### `environments.json` Reference

The file is read by `infra/app.ts`. Top-level fields apply to all environments; per-environment fields are nested under the environment name.

**Top-level fields:**

| Field | Required | Description |
|---|---|---|
| `region` | Yes | AWS region for the stack (must have Bedrock Claude access) |
| `ecr_repo_name` | Yes | ECR repository name (e.g., `ccag`) |
| `stack_name` | No | CloudFormation stack name (default: `CCAG`) |

**Per-environment fields** (nested under `staging`, `prod`, or any name you choose):

| Field | Required | Description |
|---|---|---|
| `account_id` | Yes | AWS account ID |
| `desired_count` | Yes | Number of ECS tasks (min capacity for autoscaling) |
| `domain_name` | No | Full domain name (e.g., `ccag.example.com`). Enables HTTPS, Route53 record, and sets `OIDC_AUDIENCE` |
| `hosted_zone_name` | No | Route53 hosted zone (e.g., `example.com`). Required for auto-created ACM cert and DNS record |
| `certificate_arn` | No | Existing ACM certificate ARN. If omitted and `domain_name` + `hosted_zone_name` are set, a cert is auto-created via DNS validation |
| `admin_users` | No | Comma-separated OIDC subjects auto-provisioned as admin users |
| `admin_password` | No | Admin login password (default: `admin`). **Change this for production.** |
| `rds_iam_auth` | No | Use IAM authentication for RDS instead of Secrets Manager password (default: `false`). Requires a manual `GRANT rds_iam TO proxy;` after first deploy. |
| `allowed_cidrs` | No | JSON array of CIDR blocks allowed to reach the ALB (e.g., `["203.0.113.0/24"]`). When `null` or omitted, ALB is open to all sources. |

**How TLS is determined:**
- If `certificate_arn` is provided, that cert is used with HTTPS on port 443.
- If `domain_name` and `hosted_zone_name` are set (but no `certificate_arn`), an ACM cert is auto-created via DNS validation.
- If neither is set, the ALB listens on HTTP port 80 (not recommended for production).

### CDK Context Parameters

Pass these with `-c key=value` on the command line:

| Parameter | Description |
|---|---|
| `environment` | Which environment block to use from `environments.json` (default: `prod`) |
| `imageTag` | ECR image tag to deploy (e.g., `abcd1234`) |
| `imageDigest` | ECR image digest (`sha256:...`). Preferred over `imageTag` to avoid no-op deploys |
| `rdsIamAuth` | Use IAM auth for RDS (`true`/`false`). Overrides `rds_iam_auth` in environments.json. |
| `alarmWebhookUrl` | Webhook URL for alarm notifications (Slack, etc.) via EventBridge API destination |

The `alarmWebhookUrl` can also be set via the `ALARM_WEBHOOK_URL` environment variable.

### Environment Variables on the Container

These are set automatically by the CDK stack on the Fargate tasks:

| Variable | Source | Description |
|---|---|---|
| `PROXY_HOST` | Hardcoded `0.0.0.0` | Listen address |
| `PROXY_PORT` | Hardcoded `8080` | Listen port |
| `RUST_LOG` | Hardcoded `info` | Log level |
| `LOG_FORMAT` | Hardcoded `json` | Structured logging for CloudWatch |
| `DATABASE_HOST` | From RDS endpoint | Postgres host |
| `DATABASE_PORT` | From RDS endpoint | Postgres port |
| `DATABASE_NAME` | Hardcoded `proxy` | Postgres database name |
| `DATABASE_USER` | Hardcoded `proxy` | Postgres username |
| `DB_PASSWORD` | From Secrets Manager (default) | Postgres password (injected as ECS secret). Only set when using Secrets Manager auth (the default). |
| `RDS_IAM_AUTH` | CDK context flag | Set to `true` when deployed with `-c rdsIamAuth=true`. Uses IAM auth tokens instead of password. |
| `OIDC_AUDIENCE` | From `domain_name` | OIDC JWT audience claim (if domain configured) |
| `ADMIN_USERS` | From `admin_users` | OIDC subjects with admin access |
| `ADMIN_PASSWORD` | From `admin_password` | Admin login password (if set in environments.json) |

To add additional environment variables (e.g., `OIDC_ISSUER`, `OTEL_EXPORTER_OTLP_ENDPOINT`), modify the `environment` block in `infra/stack.ts` where the container is defined.

### Multiple Environments

You can define multiple environments in `environments.json`:

```json
{
  "region": "us-west-2",
  "ecr_repo_name": "ccag",
  "staging": {
    "account_id": "111111111111",
    "desired_count": 1,
    "domain_name": "ccag-staging.example.com",
    "hosted_zone_name": "example.com"
  },
  "prod": {
    "account_id": "222222222222",
    "desired_count": 2,
    "domain_name": "ccag.example.com",
    "hosted_zone_name": "example.com"
  }
}
```

Deploy to a specific environment:

```bash
npx cdk deploy -c environment=staging -c imageTag=abcd1234
npx cdk deploy -c environment=prod -c imageTag=abcd1234
```

## Upgrading

1. **Pull latest code:**
   ```bash
   git pull
   ```

2. **Check for breaking changes** in the changelog or release notes.

3. **Update CDK dependencies** (if `package.json` changed):
   ```bash
   cd infra && npm install
   ```

4. **Build and push the new Docker image:**
   ```bash
   IMAGE_TAG=$(git rev-parse --short=8 HEAD)
   docker buildx build --platform linux/arm64 -t ccag .
   docker tag ccag:latest <account>.dkr.ecr.<region>.amazonaws.com/ccag:${IMAGE_TAG}
   docker push <account>.dkr.ecr.<region>.amazonaws.com/ccag:${IMAGE_TAG}
   ```

5. **Deploy:**
   ```bash
   cd infra
   npx cdk deploy -c environment=prod -c imageTag=${IMAGE_TAG}
   ```

   CloudFormation computes the delta and only updates changed resources. ECS performs a rolling deployment: new tasks start, pass health checks, then old tasks drain.

6. **Database migrations** run automatically on application startup. No manual migration step is needed.

7. **Rollback:** If deployment fails, CloudFormation automatically rolls back to the previous state. To manually roll back to a previous image:
   ```bash
   npx cdk deploy -c environment=prod -c imageTag=<previous-tag>
   ```

## Monitoring

### CloudWatch Alarms

The stack creates these alarms, all publishing to the SNS alarm topic:

| Alarm | Condition | Description |
|---|---|---|
| **ALB 5xx** | > 5 in 5 min | ALB-generated errors (e.g., 504 timeouts) |
| **Target 5xx** | > 10 in 5 min | Application-generated 5xx errors |
| **Unhealthy Targets** | >= 1 for 2 min | ECS tasks failing health checks |
| **High Latency** | p99 > 120s for 15 min | Sustained extreme response times |
| **DB CPU** | > 80% for 15 min | RDS CPU utilization |
| **DB Storage** | < 2 GB | RDS free storage space |
| **DB Connections** | > 80 for 10 min | Approaching RDS connection limit (~120 for t4g.small) |
| **App Errors** | > 5 in 5 min | Log-based: ERROR or panic in application logs |

**Subscribe to alarms via email:**
```bash
aws sns subscribe --topic-arn <AlarmTopicArn> --protocol email --notification-endpoint you@example.com
```

**Webhook notifications (Slack, PagerDuty, etc.):** Pass `alarmWebhookUrl` during deploy. CloudWatch alarm state changes are forwarded via EventBridge to your webhook as JSON:
```json
{
  "alarmName": "CCAG-Alb5xxAlarm",
  "state": "ALARM",
  "reason": "Threshold crossed: 8 > 5",
  "description": "ALB is generating 5xx errors",
  "timestamp": "2025-01-15T10:30:00Z"
}
```

### CloudWatch Logs

- **Log group:** Created automatically with 1 month retention
- **Log stream prefix:** Uses the ECR repo name (e.g., `ccag`)
- **View logs:**
  ```bash
  aws logs tail /aws/ecs/ccag --follow
  ```
  Or find the log group name in the CloudFormation stack outputs/resources.

### Health Check Endpoints

| Endpoint | Description |
|---|---|
| `GET /health` | Basic liveness check (used by ALB target group, 15s interval) |

### Prometheus Metrics

The gateway exposes Prometheus metrics at `GET /metrics`. To scrape these, configure a Prometheus instance or use Amazon Managed Prometheus with an ECS sidecar.

To export metrics via OTLP, set `OTEL_EXPORTER_OTLP_ENDPOINT` in the container environment.

### RDS Monitoring

- **Performance Insights** is enabled. View query-level metrics in the RDS console.
- **PostgreSQL logs** are exported to CloudWatch Logs
- **Enhanced Monitoring** at 60-second granularity

## Teardown

1. **Disable RDS deletion protection** (the stack enables it by default):
   ```bash
   aws rds modify-db-instance \
     --db-instance-identifier <instance-id> \
     --no-deletion-protection
   ```

2. **Destroy the stack:**
   ```bash
   cd infra
   npx cdk destroy -c environment=prod
   ```
   Note: RDS has `removalPolicy: SNAPSHOT`, so a final snapshot is created before deletion.

3. **Clean up ECR images** (ECR repository is not managed by the stack):
   ```bash
   aws ecr delete-repository --repository-name ccag --force
   ```

4. **Clean up CloudWatch log groups** (not deleted by `cdk destroy` due to RETAIN policy):
   ```bash
   # List CCAG-related log groups
   aws logs describe-log-groups --query "logGroups[?contains(logGroupName, 'CCAG')].logGroupName" --output table

   # Delete each one
   aws logs delete-log-group --log-group-name <log-group-name>
   ```
   Log groups that may remain: application log group (RETAIN policy), Container Insights performance logs, and RDS PostgreSQL export logs.

5. **Clean up RDS snapshots** (created by SNAPSHOT removal policy):
   ```bash
   # List snapshots
   aws rds describe-db-snapshots --query "DBSnapshots[?contains(DBSnapshotIdentifier, 'ccag') || contains(DBSnapshotIdentifier, 'CCAG')].{ID:DBSnapshotIdentifier,Size:AllocatedStorage}" --output table

   # Delete each snapshot
   aws rds delete-db-snapshot --db-snapshot-identifier <snapshot-id>
   ```

6. **Clean up the CDK bootstrap stack** (optional, if no other CDK stacks in the account):
   ```bash
   aws cloudformation delete-stack --stack-name CDKToolkit
   ```

## Troubleshooting

### Bedrock Access Denied

**Symptom:** Gateway returns 403 or "access denied" errors from Bedrock.

**Fix:** Ensure Bedrock model access is enabled in your AWS account for the target region. Go to the Bedrock console > Model access > Enable the Claude models you need. Note that newer models (Claude 4.5, 4.6) require inference profiles, which the gateway handles automatically.

### Wrong Region

**Symptom:** Model not found or empty responses.

**Fix:** The gateway auto-detects model routing prefixes from the AWS region. Ensure your `region` in `environments.json` matches a region where Claude models are available. Supported regions include `us-west-2`, `us-east-1`, `eu-west-1`, `ap-southeast-2`, and others.

### ECS Tasks Not Starting

**Symptom:** Desired count > 0 but no running tasks.

**Check:**
```bash
# View stopped task reasons
aws ecs describe-tasks --cluster <cluster-name> \
  --tasks $(aws ecs list-tasks --cluster <cluster-name> --desired-status STOPPED --query 'taskArns[0]' --output text) \
  --query 'tasks[0].{reason:stoppedReason,status:lastStatus}'

# View task logs
aws logs tail <log-group-name> --since 30m
```

Common causes:
- ECR image not found (wrong tag or region)
- Health check failing (container crash, DB connection failure)
- Insufficient Fargate capacity (rare)

### Connecting to RDS

The stack enables ECS Exec on Fargate tasks. The helper script fetches DB credentials locally (IAM auth token or Secrets Manager password) and runs `psql` inside the container:

```bash
.claude/scripts/bastion.sh connect --env prod       # Interactive psql
.claude/scripts/bastion.sh query --env prod 'SELECT count(*) FROM api_keys'
.claude/scripts/bastion.sh shell --env prod          # Interactive shell
```

### ALB 504 Timeout Errors

**Symptom:** Intermittent 504 Gateway Timeout on long-running requests.

**Context:** The ALB idle timeout is set to 900 seconds (15 minutes) to accommodate streaming responses from thinking models, which can have long pauses before the first chunk. If you still see 504s, the Bedrock request itself may be timing out.

### CloudFormation Rollback

If a deployment fails, CloudFormation automatically rolls back. To investigate:

```bash
aws cloudformation describe-stack-events --stack-name CCAG \
  --query 'StackEvents[?ResourceStatus==`CREATE_FAILED` || ResourceStatus==`UPDATE_FAILED`]'
```

## See Also

- [Getting Started](../docs/getting-started.md): initial setup walkthrough
- [Configuration](../docs/configuration.md): environment variables and runtime settings
- [Authentication](../docs/authentication.md): OIDC provider setup
- [Upgrading](../docs/upgrading.md): upgrade and rollback procedures
