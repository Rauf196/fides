-- accounts: balance holders
-- normal_balance is derived from account_type, not stored
CREATE TABLE accounts (
    id              BIGSERIAL PRIMARY KEY,
    account_type    SMALLINT NOT NULL,
    asset_code      VARCHAR(16) NOT NULL,
    asset_scale     SMALLINT NOT NULL,
    version         INTEGER NOT NULL DEFAULT 0,
    created_at      BIGINT NOT NULL,

    CONSTRAINT chk_account_type CHECK (account_type BETWEEN 1 AND 5),
    CONSTRAINT chk_asset_scale CHECK (asset_scale BETWEEN 0 AND 18),
    CONSTRAINT chk_version CHECK (version >= 0),
    CONSTRAINT chk_created_at CHECK (created_at > 0)
);

-- transactions: containers for entries
CREATE TABLE transactions (
    id              BIGSERIAL PRIMARY KEY,
    idempotency_key VARCHAR(64) NOT NULL UNIQUE,
    status          SMALLINT NOT NULL,
    metadata        JSONB NOT NULL,
    created_at      BIGINT NOT NULL,
    posted_at       BIGINT,

    CONSTRAINT chk_status CHECK (status BETWEEN 1 AND 4),
    CONSTRAINT chk_created_at CHECK (created_at > 0),
    CONSTRAINT chk_posted_at CHECK (posted_at IS NULL OR posted_at > 0)
);

-- entries: append-only ledger (NEVER update status after Posted/Voided)
CREATE TABLE entries (
    id              BIGSERIAL PRIMARY KEY,
    transaction_id  BIGINT NOT NULL REFERENCES transactions(id),
    account_id      BIGINT NOT NULL REFERENCES accounts(id),
    entry_type      SMALLINT NOT NULL,
    amount          BIGINT NOT NULL,
    status          SMALLINT NOT NULL,
    created_at      BIGINT NOT NULL,

    CONSTRAINT chk_entry_type CHECK (entry_type BETWEEN 1 AND 2),
    CONSTRAINT chk_amount CHECK (amount > 0),
    CONSTRAINT chk_status CHECK (status BETWEEN 1 AND 3),
    CONSTRAINT chk_created_at CHECK (created_at > 0)
);

-- indexes for foreign key columns (PostgreSQL doesn't auto-create these)
CREATE INDEX idx_entries_transaction_id ON entries(transaction_id);
CREATE INDEX idx_entries_account_id ON entries(account_id);
