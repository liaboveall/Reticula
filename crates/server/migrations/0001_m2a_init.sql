CREATE TABLE users (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    avatar_url TEXT NOT NULL,
    storage_bytes BIGINT NOT NULL DEFAULT 0 CHECK (storage_bytes >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE identities (
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    subject TEXT NOT NULL,
    PRIMARY KEY (provider, subject)
);

CREATE TABLE sessions (
    token_hash BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE workbooks (
    id UUID PRIMARY KEY,
    owner_id UUID NOT NULL REFERENCES users (id),
    title TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility = 'private'),
    log_epoch SMALLINT NOT NULL DEFAULT 0 CHECK (log_epoch BETWEEN 0 AND 255),
    next_per_epoch_replica INTEGER NOT NULL DEFAULT 2 CHECK (next_per_epoch_replica BETWEEN 1 AND 65535),
    log_bytes BIGINT NOT NULL DEFAULT 0 CHECK (log_bytes >= 0),
    est_bytes BIGINT NOT NULL DEFAULT 0 CHECK (est_bytes >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE members (
    workbook_id UUID NOT NULL REFERENCES workbooks (id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('owner', 'editor')),
    PRIMARY KEY (workbook_id, user_id)
);

CREATE TABLE share_links (
    token_hash BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    workbook_id UUID NOT NULL REFERENCES workbooks (id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role = 'editor'),
    created_by UUID NOT NULL REFERENCES users (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ
);

CREATE TABLE replicas (
    workbook_id UUID NOT NULL REFERENCES workbooks (id) ON DELETE CASCADE,
    replica INTEGER NOT NULL CHECK (replica BETWEEN 1 AND 16777214 AND (replica & 65535) BETWEEN 1 AND 65534),
    user_id UUID NOT NULL REFERENCES users (id),
    last_client_seq BIGINT NOT NULL DEFAULT 0 CHECK (last_client_seq >= 0),
    max_id_counter BIGINT NOT NULL DEFAULT 0 CHECK (max_id_counter BETWEEN 0 AND 4294967295),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workbook_id, replica),
    UNIQUE (workbook_id, replica, user_id)
);

CREATE TABLE branches (
    id UUID NOT NULL,
    workbook_id UUID NOT NULL REFERENCES workbooks (id) ON DELETE CASCADE,
    parent_id UUID,
    fork_seq BIGINT NOT NULL DEFAULT 0 CHECK (fork_seq >= 0),
    kind TEXT NOT NULL DEFAULT 'main',
    status TEXT NOT NULL DEFAULT 'active',
    created_by UUID NOT NULL REFERENCES users (id),
    PRIMARY KEY (workbook_id, id),
    FOREIGN KEY (workbook_id, parent_id) REFERENCES branches (workbook_id, id)
);

CREATE TABLE txns (
    workbook_id UUID NOT NULL,
    branch_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq > 0),
    replica INTEGER NOT NULL,
    client_seq BIGINT NOT NULL CHECK (client_seq > 0),
    base_seq BIGINT NOT NULL CHECK (base_seq >= 0 AND base_seq < seq),
    user_id UUID NOT NULL REFERENCES users (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload BYTEA NOT NULL,
    PRIMARY KEY (workbook_id, branch_id, seq),
    UNIQUE (workbook_id, branch_id, replica, client_seq),
    FOREIGN KEY (workbook_id, branch_id) REFERENCES branches (workbook_id, id) ON DELETE CASCADE,
    FOREIGN KEY (workbook_id, replica, user_id) REFERENCES replicas (workbook_id, replica, user_id)
);

CREATE TABLE snapshots (
    workbook_id UUID NOT NULL,
    branch_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq >= 0),
    format_version INTEGER NOT NULL CHECK (format_version >= 0),
    storage_key TEXT NOT NULL,
    size_bytes BIGINT NOT NULL CHECK (size_bytes > 0),
    sha256 BYTEA NOT NULL CHECK (octet_length(sha256) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workbook_id, branch_id, seq),
    FOREIGN KEY (workbook_id, branch_id) REFERENCES branches (workbook_id, id) ON DELETE CASCADE
);
