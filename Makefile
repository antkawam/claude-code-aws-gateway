.PHONY: test lint fmt test-integration test-e2e test-frontend coverage check build help

help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'

build: ## Build gateway and CLI
	cargo build --workspace

build-release: ## Build release binaries
	cargo build --workspace --release

test: ## Run unit tests
	cargo test --workspace --lib

lint: ## Check formatting and run clippy
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt: ## Auto-format code
	cargo fmt --all

test-integration: ## Run integration tests (requires Docker)
	@docker compose -f docker-compose.test.yml up -d --wait 2>/dev/null || \
		(docker-compose -f docker-compose.test.yml up -d 2>/dev/null && \
		echo "Waiting for postgres..." && \
		for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do \
			docker compose -f docker-compose.test.yml exec -T postgres-test pg_isready -U proxy -d proxy 2>/dev/null && break || sleep 1; \
		done)
	@echo "Running integration tests..."
	@SQLX_OFFLINE=true DATABASE_URL=postgres://proxy:testpass@127.0.0.1:5433/proxy \
		cargo test --features integration; \
		status=$$?; \
		docker compose -f docker-compose.test.yml down 2>/dev/null || \
			docker-compose -f docker-compose.test.yml down 2>/dev/null; \
		exit $$status

test-e2e: ## Run e2e HTTP tests (requires Docker + AWS credentials)
	@echo "Starting test database..."
	@docker compose -f docker-compose.test.yml up -d --wait 2>/dev/null || \
		(docker-compose -f docker-compose.test.yml up -d 2>/dev/null && \
		echo "Waiting for postgres..." && \
		for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do \
			docker compose -f docker-compose.test.yml exec -T postgres-test pg_isready -U proxy -d proxy 2>/dev/null && break || sleep 1; \
		done)
	@echo "Building gateway..."
	@cargo build --bin ccag-server
	@echo "Starting gateway..."
	@DATABASE_URL=postgres://proxy:testpass@127.0.0.1:5433/proxy \
		./target/debug/ccag-server & \
		GW_PID=$$!; \
		sleep 2; \
		echo "Running e2e tests..."; \
		./tests/e2e/test_cc_compat.sh http://127.0.0.1:8080; \
		status=$$?; \
		kill $$GW_PID 2>/dev/null; \
		docker compose -f docker-compose.test.yml down 2>/dev/null || \
			docker-compose -f docker-compose.test.yml down 2>/dev/null; \
		exit $$status

check: lint test test-integration ## Run all checks (what CI runs)

coverage: ## Generate code coverage report (requires cargo-tarpaulin)
	cargo tarpaulin --workspace --lib --out html --output-dir coverage --skip-clean

test-frontend: ## Run Playwright frontend tests (requires running gateway)
	@docker compose -f docker-compose.test.yml up -d --wait 2>/dev/null || \
		(docker-compose -f docker-compose.test.yml up -d 2>/dev/null && \
		echo "Waiting for postgres..." && \
		for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do \
			docker compose -f docker-compose.test.yml exec -T postgres-test pg_isready -U proxy -d proxy 2>/dev/null && break || sleep 1; \
		done)
	@echo "Building gateway..."
	@cargo build --bin ccag-server
	@echo "Starting gateway..."
	@DATABASE_URL=postgres://proxy:testpass@127.0.0.1:5433/proxy \
		ADMIN_PASSWORD_ENABLE=true \
		./target/debug/ccag-server & \
		GW_PID=$$!; \
		for i in 1 2 3 4 5; do curl -sf http://127.0.0.1:8080/health && break || sleep 2; done; \
		echo "Running frontend tests..."; \
		cd tests/frontend && npm install && npx playwright install chromium && npx playwright test; \
		status=$$?; \
		kill $$GW_PID 2>/dev/null; \
		docker compose -f docker-compose.test.yml down 2>/dev/null || \
			docker-compose -f docker-compose.test.yml down 2>/dev/null; \
		exit $$status

dev: ## Start local dev environment (Postgres + gateway)
	docker compose up -d postgres
	@PG_PORT=$${POSTGRES_PORT:-5432}; \
	echo "Postgres running on port $${PG_PORT}. Start the gateway with:"; \
	echo "  DATABASE_URL=postgres://proxy:devpass@127.0.0.1:$${PG_PORT}/proxy cargo run"

dev-down: ## Stop local dev environment
	docker compose down
