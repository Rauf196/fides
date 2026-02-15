<p align="center">
  <img src="docs/logos/logo-alone-no-bg-upscaled.png" alt="Fides Logo" width="200">
</p>

<h1 align="center">Fides</h1>

<p align="center">
  <strong>Production-grade double-entry ledger in Rust</strong>
</p>

---

A correctness-first ledger designed to model real financial infrastructure behavior under concurrency and failure.

Double-entry bookkeeping has one invariant: every debit must equal a credit.
Fides enforces this across types, validation, storage, and runtime integrity checks.

## Guarantees

| Guarantee | Enforcement |
|-----------|-------------|
| Transactions always balance | `validate_transaction_balance()` rejects unbalanced legs before they reach storage |
| Money never appears or disappears | Background integrity checker verifies global debits == credits every 60s |
| Concurrent mutations can't corrupt state | Optimistic locking with version column, sorted lock acquisition to prevent deadlocks |
| Every write is idempotent | Client idempotency keys for authorize, transaction ID for capture/void |
| Entries are immutable | Append-only table with CHECK constraints, corrections via reversing entries |
| Balances are always recoverable | Cache rehydrates from materialized DB balances on startup |
| Arithmetic never overflows silently | All amount operations use `checked_add`/`checked_sub`, returning errors on overflow |

## Architecture

```
                    ┌──────────────────────────────────────────────┐
                    │                 fides-server                 │
                    │                                              │
 gRPC :50051 ─────────────► Service Layer (LedgerService)          │
                    │         │          │                         │
                    │         │          ▼                         │
                    │         │   BalanceCache (DashMap)           │
                    │         │    O(1) balance reads              │
                    │         │  updated after DB commit           │
                    │         │                                    │
                    │         ▼                                    │
                    │  Storage Layer (PostgresStorage)             │
                    │    │   explicit transaction handles          │
                    │    │   optimistic locking                    │
                    │    │                                         │
 HTTP :9090 ────────►  /health  /ready  /metrics                   │
                    │                                              │
                    │  Background Tasks:                           │
                    │    - Integrity checker (3 checks, 60s)       │
                    │    - Gauge poll (pool + cache stats, 15s)    │
                    └──────────────┬───────────────────────────────┘
                                   │
                                   ▼
                              PostgreSQL
                           (append-only entries)
```

| Layer | Tech | Purpose |
|-------|------|---------|
| API | gRPC (tonic) | Type-safe contracts, east-west traffic |
| HTTP | axum | Health probes, Prometheus metrics |
| Storage | PostgreSQL (sqlx) | ACID transactions, append-only entries |
| Cache | DashMap | O(1) balance reads, rehydrated on startup |

## Transaction Lifecycle

Fides uses two-phase transactions modeled after payment card authorization flows.

### Phase 1: Authorize

Creates a pending hold. Funds are reserved but not moved.

```
Client sends: idempotency_key, legs [{account_id, entry_type, amount}, ...]

1. Validate legs balance (total debits == total credits)
2. Lock accounts in sorted order (prevents deadlocks)
3. Check available balance on accounts where balance decreases
4. Create pending entries + pending transaction
5. Update materialized balances in DB (atomic with version check)
6. Commit
7. Update in-memory cache
```

### Phase 2: Capture or Void

**Capture** settles the hold. Moves amounts from pending to posted.
**Void** cancels the hold. Releases reserved funds.

```
Capture:  entries Pending → Posted,  transaction Pending → Posted
Void:     entries Pending → Voided,  transaction Pending → Voided
```

### Idempotency

| Operation | Key | Already in target state | Different terminal state |
|-----------|-----|-------------------------|--------------------------|
| Authorize | Client-provided UUID | Return existing transaction | N/A (creates new) |
| Capture | Transaction ID | Return success | Error (e.g., captured on voided) |
| Void | Transaction ID | Return success | Error (e.g., voided on captured) |

Network retries are expected. Same operation = success. Conflicting operation = explicit error.

### Balance Checking

Only accounts where balance *decreases* are checked. Deposits don't need permission.

| Normal Balance | Entry Type | Effect | Check available? |
|----------------|------------|--------|------------------|
| Debit (Asset/Expense) | Debit | Increases | No |
| Debit | Credit | **Decreases** | **Yes** |
| Credit (Liability/Equity/Revenue) | Credit | Increases | No |
| Credit | Debit | **Decreases** | **Yes** |

### Deadlock Prevention

All code paths that lock multiple accounts sort by account ID first. No circular wait = no deadlock.

```rust
account_ids.sort_by_key(|id| id.value());
account_ids.dedup();
for &id in &account_ids {
    storage.get_account_for_update(&mut tx, id).await?;
}
```

## Design Decisions

| Decision | Alternative | Why |
|----------|-------------|-----|
| `i64` + scale for money | `rust_decimal` | Homogeneous across proto, Rust, PostgreSQL. No serialization mismatch. Integer math. |
| Optimistic locking | `SELECT FOR UPDATE` everywhere | Reads don't block writes. Conflict = retry, not deadlock. |
| Materialized balances | Compute from entries on read | O(1) balance reads regardless of entry count. TigerBeetle and Stripe use the same pattern. |
| Cache update after DB commit | Cache before commit | Crash = stale cache (rehydrate on restart). Alternative = crash = wrong cache. |
| Physical tenancy | Row-level multi-tenant | Security isolation. No shared-fate risk between tenants. |
| BIGSERIAL primary keys | UUID v4/v7 | 8 bytes vs 16. Sequential writes for B-tree performance. Internal service, not public API. |
| Forever-unique idempotency | 24h window (Stripe) | Regulatory safety. "Impossible" beats "improbable after 24 hours." |
| SMALLINT + CHECK for enums | PostgreSQL ENUM type | Matches proto integer enums. Easy to extend without ALTER TYPE migrations. |
| Fine-grained storage API | Coarse-grained (hidden transactions) | Service layer controls atomicity. Auditor can read the code and see exactly what's atomic. |
| Real PostgreSQL for tests | Mocks | Tests constraint enforcement, transaction isolation, optimistic locking. Bank-grade realism. |
| `Option<T>` for not-found | `StorageError::NotFound` | Absence isn't always an error. Service layer decides the semantics. |
| Intentional error mapping | Generic error strings | gRPC status codes chosen per spec (ABORTED = retry, FAILED_PRECONDITION = state issue). Internal errors logged server-side, generic message to client. |

## gRPC API

| RPC | Purpose | Notes |
|-----|---------|-------|
| `CreateAccount` | Create a new account | Initializes cache with zero balance |
| `GetAccount` | Fetch account by ID | |
| `GetBalance` | Get posted/pending/available | O(1) from cache |
| `Authorize` | Phase 1: create pending hold | Idempotent via `idempotency_key` |
| `Capture` | Phase 2: settle pending → posted | Idempotent (already captured = success) |
| `Void` | Phase 2: release pending hold | Idempotent (already voided = success) |
| `GetEntries` | Stream entries for an account | Server-streaming RPC for audit |

Full protobuf definition: [`fides-proto/proto/ledger.proto`](fides-proto/proto/ledger.proto)

## Getting Started

### Prerequisites

- Rust (stable)
- PostgreSQL 15+

### Build

```bash
cargo build
```

### Configure

Copy the example config and adjust:

```bash
cp config.example.toml config.toml
cp .env.example .env
# edit .env with your DATABASE_URL
```

Config precedence: defaults → `config.toml` → `FIDES__*` env vars → `DATABASE_URL`

### Run

```bash
# run migrations, then start the server
cargo run -- serve --run-migrations

# or separately (production pattern, avoids race conditions in multi-instance deployments)
cargo run -- migrate
cargo run -- serve
```

The server binds gRPC on `:50051` and HTTP (health/metrics) on `:9090` by default.

### Configuration Reference

```toml
[server]
grpc_port = 50051
http_port = 9090
shutdown_timeout_secs = 30

[database]
url = "postgres://localhost/fides"
max_connections = 10
min_connections = 1
acquire_timeout_secs = 5
idle_timeout_secs = 600

[logging]
level = "info"        # trace, debug, info, warn, error
format = "pretty"     # pretty or json

[observability]
integrity_check_interval_secs = 60  # minimum 10
```

All fields can be overridden with `FIDES__<SECTION>__<FIELD>` env vars.

## Observability

### Prometheus Metrics

Available at `GET /metrics` on the HTTP port.

| Metric | Type | Labels | Purpose |
|--------|------|--------|---------|
| `fides_grpc_requests_total` | Counter | `method`, `status` | Per-RPC request count |
| `fides_grpc_request_duration_seconds` | Histogram | `method` | Per-RPC latency |
| `fides_transactions_total` | Counter | `status` | Transaction outcomes |
| `fides_insufficient_funds_total` | Counter | | Rejected authorizations |
| `fides_idempotent_hits_total` | Counter | `method` | Deduplicated requests |
| `fides_db_pool_size` | Gauge | | Active connections |
| `fides_db_pool_idle` | Gauge | | Idle connections |
| `fides_balance_cache_accounts` | Gauge | | Cached account count |
| `fides_integrity_global_balanced` | Gauge | | 1 = debits == credits, 0 = violation |
| `fides_integrity_account_mismatches` | Gauge | | Accounts with stale materialized balance |
| `fides_integrity_cache_mismatches` | Gauge | | Cache/DB divergence count |
| `fides_integrity_check_duration_seconds` | Histogram | `check` | Per-check latency |
| `fides_integrity_last_check_timestamp` | Gauge | | Unix timestamp of last check |

### Integrity Checker

A background task runs three financial integrity checks on a configurable interval:

1. **Global balance**: verifies total debits == total credits across all non-voided entries. If this gauge ever reads 0, something is fundamentally broken.
2. **Per-account reconciliation**: recomputes each account's balance from entries and compares against the materialized `posted_balance`/`pending_balance` columns.
3. **Cache consistency**: compares in-memory cache with database. Transient mismatches are expected under load (non-atomic read). Persistent non-zero across multiple intervals = investigate.

**Known limitation:** The global check can miss paired imbalances that cancel out (transaction A: +1 debit imbalance, transaction B: +1 credit imbalance = globally balanced). This is acceptable because per-transaction balance is enforced at write time. The background check is a safety net for bugs in that enforcement.

### Health Probes

| Endpoint | Purpose | Failure |
|----------|---------|---------|
| `GET /health` | Liveness probe | Never fails (if process is running) |
| `GET /ready` | Readiness probe | 503 if DB unreachable or shutting down |

## Testing

Comprehensive unit, property, and integration tests covering invariants, storage correctness, and concurrent behavior.

| Category | What it covers |
|----------|----------------|
| Unit tests | Domain types, validation, config, metrics classification |
| Property tests | Double-entry invariants (balanced transactions, commutative balance, delta consistency) |
| Storage integration | PostgreSQL CRUD, optimistic locking, balance reconciliation |
| gRPC integration | Full RPC lifecycle, idempotency, concurrency, HTTP probes |

### Running Tests

```bash
# unit tests (no database needed)
cargo test --lib

# property tests (no database needed)
cargo test --test property_tests

# integration tests (requires running PostgreSQL)
cargo test --test storage_integration
cargo test --test grpc_integration

# all tests
cargo test
```

### Benchmarks

5 Criterion benchmarks covering cache operations, balance computation, and validation hot paths.

```bash
cargo bench
```

### Load Testing

The `fides-load` workspace member runs concurrent authorize+capture operations against a running server.

```bash
# start the server first
cargo run --release -- serve --run-migrations

# in another terminal
cargo run --release -p fides-load -- \
  --target http://127.0.0.1:50051 \
  --accounts 20 \
  --concurrency 8 \
  --duration 30
```

Reports throughput (ops/sec), rejection rate, and latency percentiles (p50/p95/p99/max).

## Project Structure

```
fides/
├── fides-proto/              # protobuf definitions + generated code
│   └── proto/ledger.proto
├── fides-server/             # main server crate
│   ├── src/
│   │   ├── main.rs           # CLI, tracing init, signal handling
│   │   ├── lib.rs            # library root
│   │   ├── config.rs         # layered config (toml/env/cli)
│   │   ├── server.rs         # DB connect, migrations, serve()
│   │   ├── health.rs         # /health and /ready endpoints
│   │   ├── domain/           # core types with compile-time guarantees
│   │   │   ├── money.rs      # Amount with checked arithmetic
│   │   │   ├── account.rs    # AccountId, AccountType, NormalBalance
│   │   │   ├── entry.rs      # EntryId, EntryType, EntryStatus
│   │   │   ├── transaction.rs # TransactionId, TransactionStatus
│   │   │   └── validation.rs # TransferLeg, balance computation
│   │   ├── storage/          # persistence layer
│   │   │   ├── postgres.rs   # PostgresStorage (all SQL)
│   │   │   └── cache.rs      # BalanceCache (DashMap)
│   │   ├── service/          # gRPC handlers
│   │   │   └── ledger.rs     # LedgerService impl
│   │   └── observability/    # metrics + monitoring
│   │       ├── metrics.rs    # Prometheus recorder + /metrics
│   │       ├── grpc_metrics.rs # Tower middleware for per-RPC metrics
│   │       └── integrity.rs  # Background integrity checker
│   ├── tests/                # integration tests
│   │   ├── storage_integration.rs
│   │   ├── grpc_integration.rs
│   │   └── property_tests.rs
│   └── benches/
│       └── ledger.rs         # Criterion benchmarks
├── fides-load/               # load testing tool
├── migrations/               # SQL schema (embedded in binary)
└── config.example.toml
```

## Limitations

Things intentionally not implemented (scope, not oversight):

- **No authentication/authorization**: Fides is an internal ledger service. Auth belongs at the API gateway.
- **No partial capture**: Capture settles the full authorized amount. Partial capture would require amount tracking per entry.
- **No multi-currency transactions**: Each account has a fixed `asset_code`. Cross-currency requires a separate FX service.
- **No event sourcing / CDC**: State is materialized. An event bus (Kafka, NATS) would be a separate integration layer.
- **No horizontal scaling**: Single-instance design (physical tenancy). Scaling = shard by tenant at the deployment level.

## License

MIT
