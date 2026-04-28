#!/bin/bash
# Start local dev environment: Postgres + gateway binary.
#
# Usage (or via make targets):
#   scripts/dev.sh                # Start Postgres + gateway (foreground)
#   scripts/dev.sh --port 8081   # Override gateway port
#   scripts/dev.sh --build       # Force cargo build before starting
#   scripts/dev.sh --seed        # Seed mock data for org analytics testing
#   scripts/dev.sh --reset       # Wipe Postgres volume and start fresh
#   scripts/dev.sh --bg          # Start in background, wait for healthcheck, exit
#   scripts/dev.sh stop          # Stop Postgres (gateway is foreground, Ctrl-C)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"
export COMPOSE_PROJECT_NAME="ccag-$(basename "$REPO_ROOT")"

if [[ -n "${POSTGRES_PORT:-}" ]]; then
    PG_PORT="$POSTGRES_PORT"
else
    PG_PORT=5432
    while lsof -i ":${PG_PORT}" &>/dev/null; do
        echo "Postgres port ${PG_PORT} in use, trying $((PG_PORT + 1))..."
        PG_PORT=$((PG_PORT + 1))
        if [[ $PG_PORT -gt 5449 ]]; then
            echo "ERROR: No free Postgres port found in 5432-5449" >&2
            exit 1
        fi
    done
fi
export POSTGRES_PORT="$PG_PORT"
PG_PASSWORD="${POSTGRES_PASSWORD:-devpass}"
GATEWAY_PORT=""
FORCE_BUILD=false
SEED_DATA=false
BACKGROUND=false
SUBCOMMAND=""

# ---- Parse args ----
while [[ $# -gt 0 ]]; do
    case "$1" in
        stop)       SUBCOMMAND="stop"; shift ;;
        --port)     GATEWAY_PORT="$2"; shift 2 ;;
        --build)    FORCE_BUILD=true; shift ;;
        --seed)     SEED_DATA=true; shift ;;
        --reset)    SUBCOMMAND="reset"; shift ;;
        --bg)       BACKGROUND=true; shift ;;
        *)          echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done

DATABASE_URL="postgres://proxy:${PG_PASSWORD}@127.0.0.1:${PG_PORT}/proxy"

# ---- Subcommands ----
if [[ "$SUBCOMMAND" == "stop" ]]; then
    echo "Stopping Postgres..."
    docker compose down
    exit 0
fi

if [[ "$SUBCOMMAND" == "reset" ]]; then
    echo "Wiping Postgres volume and restarting..."
    docker compose down -v
    docker compose up -d postgres
    echo "Waiting for Postgres..."
    for i in $(seq 1 30); do
        docker compose exec -T postgres pg_isready -U proxy -d proxy 2>/dev/null && break || sleep 1
    done
    echo "Postgres ready (clean). Run again without --reset to start gateway."
    exit 0
fi

# ---- Ensure Postgres is running ----
if ! docker compose ps postgres 2>/dev/null | grep -q "running"; then
    echo "Starting Postgres..."
    docker compose up -d postgres
    echo "Waiting for Postgres to be ready..."
    for i in $(seq 1 30); do
        docker compose exec -T postgres pg_isready -U proxy -d proxy 2>/dev/null && break || sleep 1
    done
    echo "Postgres ready on port ${PG_PORT}."
else
    echo "Postgres already running on port ${PG_PORT}."
fi

# ---- Build if requested or binary missing ----
if $FORCE_BUILD || [[ ! -f target/debug/ccag-server ]]; then
    echo "Building gateway..."
    cargo build --bin ccag-server
fi

# ---- Seed mock data ----
if $SEED_DATA; then
    echo "Seeding mock data..."
    docker compose exec -T postgres psql -U proxy -d proxy <<'SQL'
-- Seed teams
INSERT INTO teams (id, name, budget_amount_usd, budget_period) VALUES
  ('a1a1a1a1-0000-0000-0000-000000000001', 'Platform', 500.00, 'monthly'),
  ('a1a1a1a1-0000-0000-0000-000000000002', 'ML Research', 1000.00, 'monthly'),
  ('a1a1a1a1-0000-0000-0000-000000000003', 'Frontend', 200.00, 'monthly')
ON CONFLICT DO NOTHING;

-- Seed users
INSERT INTO users (id, email, role, team_id) VALUES
  ('b1b1b1b1-0000-0000-0000-000000000001', 'alice@example.com', 'member', 'a1a1a1a1-0000-0000-0000-000000000001'),
  ('b1b1b1b1-0000-0000-0000-000000000002', 'bob@example.com', 'member', 'a1a1a1a1-0000-0000-0000-000000000001'),
  ('b1b1b1b1-0000-0000-0000-000000000003', 'carol@example.com', 'member', 'a1a1a1a1-0000-0000-0000-000000000002'),
  ('b1b1b1b1-0000-0000-0000-000000000004', 'dave@example.com', 'member', 'a1a1a1a1-0000-0000-0000-000000000002'),
  ('b1b1b1b1-0000-0000-0000-000000000005', 'eve@example.com', 'member', 'a1a1a1a1-0000-0000-0000-000000000003'),
  ('b1b1b1b1-0000-0000-0000-000000000006', 'frank@example.com', 'member', 'a1a1a1a1-0000-0000-0000-000000000003'),
  ('b1b1b1b1-0000-0000-0000-000000000007', 'grace@example.com', 'admin', 'a1a1a1a1-0000-0000-0000-000000000002')
ON CONFLICT DO NOTHING;

-- Seed 2000 spend_log rows across 90 days, 7 users, 5 models, 6 projects
DO $$
DECLARE
  ep_ids uuid[];
  models text[] := ARRAY['claude-sonnet-4-5','claude-opus-4-5','claude-haiku-4-5','claude-sonnet-4-6','claude-opus-4-6'];
  users text[] := ARRAY['alice@example.com','bob@example.com','carol@example.com','dave@example.com','eve@example.com','frank@example.com','grace@example.com'];
  projects text[] := ARRAY['ccag','ml-pipeline','data-dashboard','api-gateway','docs-site','internal-tools'];
  stop_reasons text[] := ARRAY['end_turn','end_turn','end_turn','end_turn','tool_use','tool_use','max_tokens','stop_sequence'];
  i integer; u_idx integer; m_idx integer; p_idx integer; sr_idx integer;
  base_time timestamptz; ep uuid;
  inp integer; outp integer; cr integer; cw integer; dur integer;
  sess text; tnames text[];
BEGIN
  SELECT array_agg(id) INTO ep_ids FROM endpoints LIMIT 5;
  IF ep_ids IS NULL THEN ep_ids := ARRAY[gen_random_uuid()]; END IF;

  FOR i IN 1..2000 LOOP
    u_idx := (i % 7) + 1; m_idx := (i % 5) + 1;
    p_idx := (i % 6) + 1; sr_idx := (i % 8) + 1;
    base_time := now() - ((random() * 90)::numeric || ' days')::interval
                       - ((random() * 23)::integer || ' hours')::interval
                       - ((random() * 59)::integer || ' minutes')::interval;
    CASE m_idx
      WHEN 1 THEN inp := 800 + (random()*5000)::integer; outp := 200 + (random()*2000)::integer;
      WHEN 2 THEN inp := 2000 + (random()*10000)::integer; outp := 500 + (random()*5000)::integer;
      WHEN 3 THEN inp := 200 + (random()*1000)::integer; outp := 50 + (random()*500)::integer;
      WHEN 4 THEN inp := 1000 + (random()*6000)::integer; outp := 300 + (random()*3000)::integer;
      WHEN 5 THEN inp := 3000 + (random()*15000)::integer; outp := 800 + (random()*8000)::integer;
      ELSE inp := 1000; outp := 500;
    END CASE;
    cr := (random() * inp * 0.6)::integer; cw := (random() * inp * 0.15)::integer;
    dur := 500 + (random() * 30000)::integer;
    ep := ep_ids[((i % array_length(ep_ids, 1)) + 1)];
    sess := 'sess-' || to_hex((i / 5)::integer);
    CASE (i % 7)
      WHEN 0 THEN tnames := ARRAY['Read','Edit','Bash','Grep'];
      WHEN 1 THEN tnames := ARRAY['Read','Write','mcp__aws-docs__search_documentation','mcp__aws-docs__read_documentation'];
      WHEN 2 THEN tnames := ARRAY['Read','Bash','web_search','Glob'];
      WHEN 3 THEN tnames := ARRAY['Read','Edit','mcp__github__create_pr','mcp__github__list_issues'];
      WHEN 4 THEN tnames := ARRAY['Read','Grep','Glob'];
      WHEN 5 THEN tnames := ARRAY['mcp__drawio__open_drawio_xml','Read','Edit'];
      WHEN 6 THEN tnames := ARRAY['Bash','Read','web_search'];
      ELSE tnames := ARRAY['Read'];
    END CASE;
    INSERT INTO spend_log (
      user_identity, model, streaming, duration_ms,
      input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
      stop_reason, tool_count, tool_names, turn_count,
      thinking_enabled, has_system_prompt,
      session_id, project_key, endpoint_id, recorded_at
    ) VALUES (
      users[u_idx], models[m_idx], (random() > 0.1), dur,
      inp, outp, cr, cw,
      stop_reasons[sr_idx], array_length(tnames, 1)::smallint, tnames, (random()*20)::smallint + 1,
      (random() > 0.3), true,
      sess, projects[p_idx], ep, base_time
    );
  END LOOP;
END $$;
SQL
    ROW_COUNT=$(docker compose exec -T postgres psql -U proxy -d proxy -t -c "SELECT COUNT(*) FROM spend_log")
    echo "Seeded. Total spend_log rows: ${ROW_COUNT}"
fi

# ---- Start gateway ----
export DATABASE_URL
export PROXY_HOST="0.0.0.0"

# Auto-detect port: use --port if given, otherwise find a free one starting at 8080
if [[ -n "$GATEWAY_PORT" ]]; then
    export PROXY_PORT="$GATEWAY_PORT"
else
    PROXY_PORT=8080
    while lsof -i ":${PROXY_PORT}" &>/dev/null; do
        echo "Port ${PROXY_PORT} in use, trying $((PROXY_PORT + 1))..."
        PROXY_PORT=$((PROXY_PORT + 1))
        if [[ $PROXY_PORT -gt 8099 ]]; then
            echo "ERROR: No free port found in 8080-8099" >&2
            exit 1
        fi
    done
    export PROXY_PORT
fi

echo ""
echo "Starting gateway..."
echo "  Database: ${DATABASE_URL}"
echo "  Portal:   http://localhost:${PROXY_PORT}/portal"
echo "  Login:    admin / admin"
echo ""

if $BACKGROUND; then
    cargo run --bin ccag-server &
    GATEWAY_PID=$!
    echo "Gateway PID: ${GATEWAY_PID}"
    for i in $(seq 1 60); do
        if curl -sf "http://localhost:${PROXY_PORT}/health" > /dev/null 2>&1; then
            echo "Gateway ready on http://localhost:${PROXY_PORT}"
            exit 0
        fi
        sleep 1
    done
    echo "ERROR: Gateway failed to start within 60s" >&2
    kill $GATEWAY_PID 2>/dev/null
    exit 1
else
    exec cargo run --bin ccag-server
fi
