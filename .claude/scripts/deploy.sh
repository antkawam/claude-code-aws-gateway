#!/bin/bash
# Build, push to GHCR, and deploy via CDK (default) or direct ECS update (--fast).
#
# Default path: build ARM64 image -> push to GHCR -> cdk deploy (handles code + infra).
# Fast path (--fast): build -> push -> direct ECS task definition update (code-only).
#
# Authenticates via AWS_PROFILE (credential_process in ~/.aws/config auto-refreshes
# via isengardcli). No eval/source needed — the AWS SDK handles credential lifecycle.
#
# Must be run from the main branch unless --force is used.
#
# Usage:
#   .claude/scripts/deploy.sh --staging              # Build + push GHCR + CDK deploy
#   .claude/scripts/deploy.sh --prod                 # Build + push GHCR + CDK deploy (production)
#   .claude/scripts/deploy.sh --staging --fast       # Build + push + direct ECS update (code-only)
#   .claude/scripts/deploy.sh --staging --skip-build # Re-deploy last built GHCR image via CDK
#   .claude/scripts/deploy.sh --staging --image URI  # Deploy a specific image via CDK
#   .claude/scripts/deploy.sh --staging --force      # Deploy from non-main branch
#   .claude/scripts/deploy.sh --prod --release       # Deploy + cut a GitHub release (tag + push)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENV_FILE="${REPO_ROOT}/environments.json"
if [ ! -f "$ENV_FILE" ]; then
    echo "ERROR: Missing ${ENV_FILE}" >&2
    echo "Create it from the template in infra/README.md" >&2
    exit 1
fi
STACK_NAME=$(jq -r '.stack_name' "$ENV_FILE")
DEPLOY_TIMEOUT=300  # 5 minutes
PRE_BUILT_IMAGE=""
SKIP_BUILD=false
FORCE=false
FAST=false
RELEASE=false
ENVIRONMENT=""

MAIN_BRANCH="main"
# Auto-detect: use "master" if it exists and "main" doesn't
if git rev-parse --verify master &>/dev/null && ! git rev-parse --verify main &>/dev/null; then
    MAIN_BRANCH="master"
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        --staging) ENVIRONMENT="staging"; shift ;;
        --prod)    ENVIRONMENT="prod"; shift ;;
        --image)   PRE_BUILT_IMAGE="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=true; shift ;;
        --force)   FORCE=true; shift ;;
        --fast)    FAST=true; shift ;;
        --release) RELEASE=true; shift ;;
        *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
done

# ---- Validate args ----
if [ -z "$ENVIRONMENT" ]; then
    echo "ERROR: Specify --staging or --prod"
    echo "Usage: .claude/scripts/deploy.sh --staging"
    exit 1
fi

BRANCH=$(git rev-parse --abbrev-ref HEAD)

# In a worktree, we're on a feature branch but want to deploy main.
# Detect main repo and resolve main's HEAD from there.
DEPLOY_REF="HEAD"
if [ "$BRANCH" != "$MAIN_BRANCH" ] && [ "$FORCE" = false ]; then
    # Check if we're in a worktree and main is checked out elsewhere
    MAIN_REPO=""
    if git worktree list --porcelain | grep -q "^worktree "; then
        while IFS= read -r line; do
            if [[ "$line" == "worktree "* ]]; then
                current_wt="${line#worktree }"
            elif [[ "$line" == "branch refs/heads/${MAIN_BRANCH}" ]]; then
                if [ "$current_wt" != "$(pwd)" ]; then
                    MAIN_REPO="$current_wt"
                fi
            fi
        done < <(git worktree list --porcelain)
    fi

    if [ -n "$MAIN_REPO" ]; then
        # Worktree: deploy from main via main repo
        DEPLOY_REF="$MAIN_BRANCH"
        echo "[worktree] Deploying ${MAIN_BRANCH} from ${MAIN_REPO}"
    else
        echo "ERROR: Must be on ${MAIN_BRANCH} branch to deploy (currently on '${BRANCH}')"
        echo "Use --force to override, or merge first: .claude/scripts/merge.sh"
        exit 1
    fi
fi

if [ "$DEPLOY_REF" = "HEAD" ] && ! git diff-index --quiet HEAD -- 2>/dev/null; then
    echo "ERROR: Working tree has uncommitted changes. Commit first."
    exit 1
fi

# ---- Read environment config ----
EXPECTED_ACCOUNT=$(jq -r ".${ENVIRONMENT}.account_id" "$ENV_FILE")
AWS_PROFILE_NAME=$(jq -r ".${ENVIRONMENT}.aws_profile" "$ENV_FILE")
REGION=$(jq -r ".region" "$ENV_FILE")
MIN_TASKS=$(jq -r ".${ENVIRONMENT}.desired_count" "$ENV_FILE")

# ---- GHCR image registry ----
GIT_REMOTE=$(jq -r '.git_remote // "origin"' "$ENV_FILE")
GHCR_OWNER=$(git remote get-url "$GIT_REMOTE" | sed 's/.*github.com[:/]\(.*\)\.git/\1/' | tr '[:upper:]' '[:lower:]')
GHCR_REPO="ghcr.io/${GHCR_OWNER}"

ENV_UPPER=$(echo "$ENVIRONMENT" | tr '[:lower:]' '[:upper:]')
DEPLOY_MODE="CDK"
[ "$FAST" = true ] && DEPLOY_MODE="ECS (fast)"
echo "==> Deploying to ${ENV_UPPER} via ${DEPLOY_MODE}"
echo "    Account: ${EXPECTED_ACCOUNT}"
echo "    Profile: ${AWS_PROFILE_NAME}"
echo "    Image:   ${GHCR_REPO}"

# ---- Authenticate via AWS profile ----
echo ""
echo "==> Authenticating via profile ${AWS_PROFILE_NAME}..."
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN 2>/dev/null || true
export AWS_PROFILE="$AWS_PROFILE_NAME"
export AWS_DEFAULT_REGION="$REGION"

# ---- Validate account ----
ACTUAL_ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
if [ "$ACTUAL_ACCOUNT" != "$EXPECTED_ACCOUNT" ]; then
    echo "    ERROR: Expected account ${EXPECTED_ACCOUNT} but got ${ACTUAL_ACCOUNT}"
    echo "    Check aws_profile '${AWS_PROFILE_NAME}' in environments.json"
    exit 1
fi
echo "    Authenticated: ${ACTUAL_ACCOUNT}"

TAG=$(git rev-parse --short=8 "$DEPLOY_REF")
echo "    Commit:  ${TAG}"

# ---- Build + Push to GHCR ----
IMAGE="${GHCR_REPO}:${TAG}"

if [ -n "$PRE_BUILT_IMAGE" ]; then
    IMAGE="$PRE_BUILT_IMAGE"
    echo "    Using pre-built image: $IMAGE"
    TAG=$(echo "$IMAGE" | rev | cut -d: -f1 | rev)
elif [ "$SKIP_BUILD" != true ]; then
    echo ""
    echo "==> Building Docker image (${TAG}, arm64)..."
    BUILD_START=$(date +%s)
    DOCKER_BUILDKIT=1 docker build \
        --platform linux/arm64 \
        -t "$IMAGE" \
        -t "${GHCR_REPO}:latest" \
        "$REPO_ROOT" 2>&1 | tail -5
    BUILD_ELAPSED=$(( $(date +%s) - BUILD_START ))
    echo "    Build complete (${BUILD_ELAPSED}s)"
else
    # --skip-build: verify the image exists (locally or on GHCR)
    if docker image inspect "$IMAGE" &>/dev/null; then
        echo "    Using existing local image: ${TAG}"
    elif docker manifest inspect "$IMAGE" &>/dev/null 2>&1; then
        echo "    Using existing GHCR image: ${TAG}"
    elif docker image inspect "${GHCR_REPO}:latest" &>/dev/null; then
        echo ""
        echo "==> Re-tagging latest -> ${TAG}"
        docker tag "${GHCR_REPO}:latest" "$IMAGE"
    else
        echo "    ERROR: No image found for ${TAG} (local or GHCR)."
        echo "    Run without --skip-build to build first."
        exit 1
    fi
fi

# Push to GHCR
if [ -z "$PRE_BUILT_IMAGE" ]; then
    echo ""
    echo "==> Pushing to GHCR..."
    GH_USER=$(gh api user -q .login 2>/dev/null || echo "")
    if [ -z "$GH_USER" ]; then
        echo "    ERROR: Not authenticated with GitHub CLI (gh). Run: gh auth login"
        exit 1
    fi
    echo "$(gh auth token)" | docker login ghcr.io -u "$GH_USER" --password-stdin 2>/dev/null
    docker push "$IMAGE" 2>&1 | grep -E "digest:|Pushed|existed" | tail -3 || true
    echo "    Pushed ${TAG}"
fi

# ---- Deploy ----
if [ "$FAST" = true ]; then
    # Fast path: direct ECS task definition update (code-only, no CloudFormation)
    echo ""
    echo "==> Discovering ECS resources..."

    # Check CloudFormation stack status — warn if rolled back
    STACK_STATUS=$(aws cloudformation describe-stacks \
        --stack-name "$STACK_NAME" \
        --query 'Stacks[0].StackStatus' --output text 2>/dev/null || echo "UNKNOWN")
    if [[ "$STACK_STATUS" == *ROLLBACK* ]]; then
        echo "    WARNING: Stack is in ${STACK_STATUS} state."
        echo "    Infrastructure changes (IAM policies, RDS settings, env vars) from the"
        echo "    last CDK deploy were rolled back. --fast only swaps the container image."
        echo "    Run without --fast to re-apply infrastructure changes."
        echo ""
    fi

    CLUSTER=$(aws cloudformation describe-stack-resources \
        --stack-name "$STACK_NAME" \
        --query "StackResources[?ResourceType=='AWS::ECS::Cluster'].PhysicalResourceId | [0]" \
        --output text)
    SERVICE=$(aws cloudformation describe-stack-resources \
        --stack-name "$STACK_NAME" \
        --query "StackResources[?ResourceType=='AWS::ECS::Service'].PhysicalResourceId | [0]" \
        --output text)

    if [ "$CLUSTER" = "None" ] || [ "$SERVICE" = "None" ]; then
        echo "    ERROR: Could not find ECS cluster/service in stack $STACK_NAME"
        echo "    Run without --fast for initial deployment."
        exit 1
    fi

    SERVICE_NAME=$(echo "$SERVICE" | rev | cut -d'/' -f1 | rev)
    echo "    Cluster: $CLUSTER"
    echo "    Service: $SERVICE_NAME"

    # Get current task definition
    TASK_DEF=$(aws ecs describe-services \
        --cluster "$CLUSTER" --services "$SERVICE_NAME" \
        --query 'services[0].taskDefinition' --output text)

    # Ensure autoscaling min capacity
    CURRENT_MIN=$(aws application-autoscaling describe-scalable-targets \
        --service-namespace ecs \
        --resource-ids "service/${CLUSTER}/${SERVICE_NAME}" \
        --query 'ScalableTargets[0].MinCapacity' --output text 2>/dev/null || echo "0")

    if [ "$CURRENT_MIN" != "None" ] && [ "$CURRENT_MIN" -lt "$MIN_TASKS" ]; then
        echo ""
        echo "==> Updating autoscaling min capacity: ${CURRENT_MIN} -> ${MIN_TASKS}"
        aws application-autoscaling register-scalable-target \
            --service-namespace ecs \
            --resource-id "service/${CLUSTER}/${SERVICE_NAME}" \
            --scalable-dimension ecs:service:DesiredCount \
            --min-capacity "$MIN_TASKS" > /dev/null
    fi

    # Deploy new task definition
    echo ""
    echo "==> Deploying (ECS direct)..."

    TASK_DEF_JSON=$(aws ecs describe-task-definition --task-definition "$TASK_DEF" --query 'taskDefinition')
    NEW_TASK_DEF=$(echo "$TASK_DEF_JSON" | \
        jq --arg IMAGE "$IMAGE" '.containerDefinitions[0].image = $IMAGE' | \
        jq '.runtimePlatform.cpuArchitecture = "ARM64"' | \
        jq '{family, taskRoleArn, executionRoleArn, networkMode, containerDefinitions, requiresCompatibilities, cpu, memory, runtimePlatform}')

    NEW_TASK_DEF_ARN=$(aws ecs register-task-definition \
        --cli-input-json "$NEW_TASK_DEF" \
        --query 'taskDefinition.taskDefinitionArn' --output text)
    NEW_TASK_REV=$(echo "$NEW_TASK_DEF_ARN" | rev | cut -d: -f1 | rev)
    echo "    Task def: revision ${NEW_TASK_REV}"

    aws ecs update-service \
        --cluster "$CLUSTER" --service "$SERVICE_NAME" \
        --task-definition "$NEW_TASK_DEF_ARN" \
        --force-new-deployment \
        --no-cli-pager > /dev/null
else
    # Default path: CDK deploy (handles code + infra changes)
    echo ""
    echo "==> Deploying via CDK..."
    cd "${REPO_ROOT}/infra"

    # Ensure dependencies are installed
    if [ ! -d "node_modules" ]; then
        echo "    Installing CDK dependencies..."
        npm install --silent
    fi

    CDK_ARGS=(-c "environment=${ENVIRONMENT}" -c "imageTag=${TAG}")

    npx cdk deploy "${CDK_ARGS[@]}" --require-approval never
fi

# ---- Wait for rollout ----
echo ""
DEPLOY_START=$(date +%s)

# For CDK path, discover ECS resources now (not needed earlier)
if [ "$FAST" != true ]; then
    CLUSTER=$(aws cloudformation describe-stack-resources \
        --stack-name "$STACK_NAME" \
        --query "StackResources[?ResourceType=='AWS::ECS::Cluster'].PhysicalResourceId | [0]" \
        --output text)
    SERVICE=$(aws cloudformation describe-stack-resources \
        --stack-name "$STACK_NAME" \
        --query "StackResources[?ResourceType=='AWS::ECS::Service'].PhysicalResourceId | [0]" \
        --output text)
    SERVICE_NAME=$(echo "$SERVICE" | rev | cut -d'/' -f1 | rev)
fi

LOG_GROUP=$(aws logs describe-log-groups --query "logGroups[?contains(logGroupName, 'CCAG')].logGroupName | [0]" --output text 2>/dev/null || echo "")

while true; do
    ELAPSED=$(( $(date +%s) - DEPLOY_START ))

    DEPLOY_JSON=$(aws ecs describe-services \
        --cluster "$CLUSTER" --services "$SERVICE_NAME" \
        --query 'services[0].deployments[?status==`PRIMARY`] | [0]' --output json)

    RUNNING=$(echo "$DEPLOY_JSON" | jq -r '.runningCount // 0')
    DESIRED=$(echo "$DEPLOY_JSON" | jq -r '.desiredCount // 0')
    FAILED=$(echo "$DEPLOY_JSON" | jq -r '.failedTasks // 0')
    ROLLOUT=$(echo "$DEPLOY_JSON" | jq -r '.rolloutState // "UNKNOWN"')

    OLD_RUNNING=$(aws ecs describe-services \
        --cluster "$CLUSTER" --services "$SERVICE_NAME" \
        --query 'services[0].deployments[?status==`ACTIVE`].runningCount | [0]' --output text 2>/dev/null || echo "0")
    [ "$OLD_RUNNING" = "None" ] && OLD_RUNNING=0

    if [ "$ROLLOUT" = "COMPLETED" ]; then
        printf "\r    Running: %s/%s                              \n" "$RUNNING" "$DESIRED"
        break
    fi

    if [ "$FAILED" -gt 0 ]; then
        printf "\r    Running: %s/%s — %s tasks failed            \n" "$RUNNING" "$DESIRED" "$FAILED"
        echo ""
        echo "==> Deploy FAILED. Recent logs:"
        if [ -n "$LOG_GROUP" ] && [ "$LOG_GROUP" != "None" ]; then
            aws logs tail "$LOG_GROUP" --since 2m 2>/dev/null | tail -15 || echo "    (could not fetch logs)"
        else
            echo "    (log group not found)"
        fi
        exit 1
    fi

    if [ "$ELAPSED" -gt "$DEPLOY_TIMEOUT" ]; then
        printf "\r    Running: %s/%s — timed out after %ss        \n" "$RUNNING" "$DESIRED" "$DEPLOY_TIMEOUT"
        echo ""
        echo "==> Deploy timed out. Check service events:"
        aws ecs describe-services --cluster "$CLUSTER" --services "$SERVICE_NAME" \
            --query 'services[0].events[:5].message' --output json
        exit 1
    fi

    if [ "$RUNNING" -eq 0 ] && [ "$OLD_RUNNING" -eq 0 ]; then
        printf "\r    Provisioning tasks... (%ss)" "$ELAPSED"
    elif [ "$OLD_RUNNING" -gt 0 ]; then
        printf "\r    Running: %s/%s new, %s old draining (%ss)" "$RUNNING" "$DESIRED" "$OLD_RUNNING" "$ELAPSED"
    else
        printf "\r    Running: %s/%s (%ss)" "$RUNNING" "$DESIRED" "$ELAPSED"
    fi

    sleep 5
done

TOTAL_ELAPSED=$(( $(date +%s) - DEPLOY_START ))
echo ""
echo "==> Deploy complete (${TOTAL_ELAPSED}s) — ${ENVIRONMENT} via ${DEPLOY_MODE}"

# ---- Post-deploy: verify running image matches deployed image ----
RUNNING_IMAGE=$(aws ecs describe-services \
    --cluster "$CLUSTER" --services "$SERVICE_NAME" \
    --query 'services[0].taskDefinition' --output text 2>/dev/null || echo "")
if [ -n "$RUNNING_IMAGE" ] && [ "$RUNNING_IMAGE" != "None" ]; then
    RUNNING_IMAGE_URI=$(aws ecs describe-task-definition --task-definition "$RUNNING_IMAGE" \
        --query 'taskDefinition.containerDefinitions[0].image' --output text 2>/dev/null || echo "")
    if [ -n "$RUNNING_IMAGE_URI" ]; then
        RUNNING_TAG=$(echo "$RUNNING_IMAGE_URI" | rev | cut -d: -f1 | rev)
        if [ "$RUNNING_TAG" != "$TAG" ]; then
            echo "    WARNING: Running image tag '${RUNNING_TAG}' differs from deployed '${TAG}'"
        else
            echo "    Image verified: ${TAG}"
        fi
    fi
fi

# ---- Post-deploy: tail logs for errors ----
if [ -n "$LOG_GROUP" ] && [ "$LOG_GROUP" != "None" ]; then
    echo ""
    echo "==> Post-deploy log check (15s)..."
    local_errors=$(aws logs filter-log-events \
        --log-group-name "$LOG_GROUP" \
        --start-time "$(( $(date +%s) * 1000 - 30000 ))" \
        --filter-pattern "ERROR" \
        --query 'events[].message' --output json 2>/dev/null || echo "[]")
    error_count=$(echo "$local_errors" | jq 'length')
    if [ "$error_count" -gt 0 ]; then
        echo "    Found ${error_count} ERROR entries in the last 30s:"
        echo "$local_errors" | jq -r '.[:5][]' | head -10
        [ "$error_count" -gt 5 ] && echo "    ... and $((error_count - 5)) more"
    else
        echo "    No errors in post-deploy logs"
    fi
fi

# ---- Release: bump version, tag, push (triggers GHA release workflow) ----
if [ "$RELEASE" = true ]; then
    if [ "$ENVIRONMENT" != "prod" ]; then
        echo ""
        echo "    WARN: --release skipped (only runs with --prod)"
    else
        echo ""
        echo "==> Cutting release..."

        # Read current version from Cargo.toml
        CURRENT_VERSION=$(grep -m1 '^version = ' "${REPO_ROOT}/Cargo.toml" | sed 's/version = "\(.*\)"/\1/')
        echo "    Current version: ${CURRENT_VERSION}"

        # Parse semver components
        MAJOR=$(echo "$CURRENT_VERSION" | cut -d. -f1)
        MINOR=$(echo "$CURRENT_VERSION" | cut -d. -f2)
        PATCH=$(echo "$CURRENT_VERSION" | cut -d. -f3)

        # Check if there's already a tag for the current version
        if git tag -l "v${CURRENT_VERSION}" | grep -q .; then
            # Version already tagged — auto-bump patch
            PATCH=$((PATCH + 1))
            NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"
            echo "    v${CURRENT_VERSION} already tagged, bumping to v${NEW_VERSION}"

            # Update Cargo.toml versions
            cd "$REPO_ROOT"
            sed -i.bak "0,/^version = \"${CURRENT_VERSION}\"/s//version = \"${NEW_VERSION}\"/" Cargo.toml && rm -f Cargo.toml.bak
            sed -i.bak "0,/^version = /s/^version = .*/version = \"${NEW_VERSION}\"/" cli/Cargo.toml && rm -f cli/Cargo.toml.bak

            # Regenerate lockfile
            cargo check --quiet 2>/dev/null || true

            # Commit version bump
            git add Cargo.toml cli/Cargo.toml Cargo.lock
            git commit -m "Bump version to v${NEW_VERSION}" --quiet
            git push origin "$MAIN_BRANCH" --quiet
        else
            NEW_VERSION="$CURRENT_VERSION"
            echo "    Using current version: v${NEW_VERSION}"
        fi

        # Create and push tag
        git tag "v${NEW_VERSION}"
        git push origin "v${NEW_VERSION}" 2>&1 | tail -2
        echo "    Tagged v${NEW_VERSION} — release workflow will build binaries"
        echo "    https://github.com/$(git remote get-url origin | sed 's/.*github.com[:/]\(.*\)\.git/\1/')/releases/tag/v${NEW_VERSION}"
    fi
fi
