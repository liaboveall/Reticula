INSERT INTO users (id, name, avatar_url, storage_bytes)
VALUES ('00000000-0000-0000-0000-000000000000', 'Deleted user', '', 0)
ON CONFLICT (id) DO NOTHING;

ALTER TABLE workbooks
    DROP CONSTRAINT workbooks_visibility_check,
    ADD CONSTRAINT workbooks_visibility_check CHECK (visibility IN ('private', 'link_view'));

ALTER TABLE members
    DROP CONSTRAINT members_role_check,
    ADD CONSTRAINT members_role_check CHECK (role IN ('owner', 'editor', 'viewer'));

ALTER TABLE share_links
    DROP CONSTRAINT share_links_role_check,
    ADD CONSTRAINT share_links_role_check CHECK (role IN ('editor', 'viewer')),
    ADD COLUMN id UUID NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN label TEXT NOT NULL DEFAULT '',
    ADD CONSTRAINT share_links_id_key UNIQUE (id);

ALTER TABLE replicas
    DROP CONSTRAINT replicas_user_id_fkey,
    ADD CONSTRAINT replicas_user_id_fkey
        FOREIGN KEY (user_id) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE branches
    DROP CONSTRAINT branches_created_by_fkey,
    ADD CONSTRAINT branches_created_by_fkey
        FOREIGN KEY (created_by) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE share_links
    DROP CONSTRAINT share_links_created_by_fkey,
    ADD CONSTRAINT share_links_created_by_fkey
        FOREIGN KEY (created_by) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED,
    DROP CONSTRAINT share_links_consumed_by_fkey,
    ADD CONSTRAINT share_links_consumed_by_fkey
        FOREIGN KEY (consumed_by) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE txns
    DROP CONSTRAINT txns_user_id_fkey,
    ADD CONSTRAINT txns_user_id_fkey
        FOREIGN KEY (user_id) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED,
    DROP CONSTRAINT txns_workbook_id_replica_user_id_fkey,
    ADD CONSTRAINT txns_workbook_id_replica_user_id_fkey
        FOREIGN KEY (workbook_id, replica, user_id)
        REFERENCES replicas (workbook_id, replica, user_id)
        DEFERRABLE INITIALLY DEFERRED;

CREATE TABLE versions (
    id UUID PRIMARY KEY,
    workbook_id UUID NOT NULL,
    branch_id UUID NOT NULL,
    seq BIGINT NOT NULL CHECK (seq >= 0),
    name TEXT NOT NULL,
    created_by UUID NOT NULL REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workbook_id, branch_id, name),
    FOREIGN KEY (workbook_id, branch_id) REFERENCES branches (workbook_id, id) ON DELETE CASCADE
);

CREATE INDEX versions_workbook_branch_seq_idx ON versions (workbook_id, branch_id, seq DESC);
CREATE INDEX workbooks_owner_updated_idx ON workbooks (owner_id, updated_at DESC);
CREATE INDEX members_user_idx ON members (user_id, workbook_id);
CREATE INDEX share_links_workbook_active_idx ON share_links (workbook_id, revoked_at);
