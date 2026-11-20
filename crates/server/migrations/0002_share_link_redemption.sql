ALTER TABLE share_links
    ADD COLUMN single_use BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN consumed_by UUID REFERENCES users (id),
    ADD CONSTRAINT share_links_consumption_requires_single_use CHECK (consumed_by IS NULL OR single_use);

CREATE INDEX sessions_expiry ON sessions (expires_at);
