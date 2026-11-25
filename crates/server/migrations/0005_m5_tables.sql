ALTER TABLE users
    ADD COLUMN table_bytes BIGINT NOT NULL DEFAULT 0 CHECK (table_bytes >= 0);

ALTER TABLE workbooks
    ADD COLUMN tables_version BIGINT NOT NULL DEFAULT 0 CHECK (tables_version >= 0);

CREATE TABLE tables (
    workbook_id UUID NOT NULL REFERENCES workbooks (id) ON DELETE CASCADE,
    id UUID NOT NULL,
    name TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    rows BIGINT NOT NULL DEFAULT 0 CHECK (rows >= 0),
    cols INTEGER NOT NULL DEFAULT 0 CHECK (cols >= 0),
    bytes BIGINT NOT NULL CHECK (bytes > 0),
    sha256 BYTEA NOT NULL CHECK (octet_length(sha256) = 32),
    status TEXT NOT NULL DEFAULT 'uploading' CHECK (status IN ('uploading', 'ready', 'missing')),
    created_by UUID NOT NULL REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workbook_id, id)
);

CREATE INDEX tables_uploading_idx ON tables (created_at) WHERE status = 'uploading';
