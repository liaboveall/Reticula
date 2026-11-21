//! Account, session, membership, share link, and quota queries.

use std::collections::BTreeMap;

use reticula_sync::MemberRole;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{DELETED_USER_ID, MAIN_BRANCH, Store, StoreError, integer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkbookVisibility {
    Private,
    LinkView,
}

impl WorkbookVisibility {
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        match raw {
            "private" => Ok(Self::Private),
            "link_view" => Ok(Self::LinkView),
            _ => Err(StoreError::Invalid("workbook visibility")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::LinkView => "link_view",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: Uuid,
    pub name: String,
    pub avatar_url: String,
}

pub struct WorkbookAccess {
    pub id: Uuid,
    pub title: String,
    pub visibility: WorkbookVisibility,
    pub epoch: u8,
    pub role: MemberRole,
    pub head: u64,
}

pub struct AnonymousShareAccess {
    pub link_id: Uuid,
    pub id: Uuid,
    pub title: String,
    pub epoch: u8,
    pub head: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbookListItem {
    pub id: Uuid,
    pub branch_id: Uuid,
    pub title: String,
    pub role: String,
    pub visibility: WorkbookVisibility,
    pub head_seq: String,
    pub est_bytes: u64,
    pub log_bytes: u64,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbookLists {
    pub owned: Vec<WorkbookListItem>,
    pub shared: Vec<WorkbookListItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemberEntry {
    pub user_id: Uuid,
    pub name: String,
    pub avatar_url: String,
    pub role: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ShareLinkRecord {
    pub id: Uuid,
    pub role: String,
    pub label: String,
    pub single_use: bool,
    pub consumed: bool,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub user: User,
    pub owned_workbooks: u64,
    pub shared_workbooks: u64,
    pub storage_bytes: u64,
    pub quota_bytes: u64,
    pub disk_over_soft_limit: bool,
    pub disk_over_hard_limit: bool,
}

pub struct DeleteAccountOutcome {
    pub owned_workbooks: Vec<Uuid>,
    pub shared_workbooks: Vec<Uuid>,
}

pub struct SessionIdentity<'a> {
    pub provider: &'a str,
    pub subject: &'a str,
    pub name: &'a str,
    pub avatar: &'a str,
}

impl Store {
    pub async fn sign_in(
        &self,
        identity: &SessionIdentity<'_>,
        session: [u8; 32],
        previous: Option<[u8; 32]>,
        session_seconds: i32,
    ) -> Result<User, StoreError> {
        let SessionIdentity {
            provider,
            subject,
            name,
            avatar,
        } = *identity;
        let mut db = self.pool.begin().await?;
        let candidate = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, name, avatar_url) VALUES ($1, $2, $3)",
            candidate,
            name,
            avatar
        )
        .execute(&mut *db)
        .await?;
        let id = sqlx::query_scalar!(
            "INSERT INTO identities (provider, subject, user_id) VALUES ($1, $2, $3)
             ON CONFLICT (provider, subject) DO UPDATE SET subject = EXCLUDED.subject RETURNING user_id",
            provider, subject, candidate,
        )
        .fetch_one(&mut *db)
        .await?;
        if id == DELETED_USER_ID {
            return Err(StoreError::Forbidden);
        }
        if id != candidate {
            sqlx::query!(
                "UPDATE users SET name = $2, avatar_url = $3 WHERE id = $1",
                id,
                name,
                avatar
            )
            .execute(&mut *db)
            .await?;
            sqlx::query!("DELETE FROM users WHERE id = $1", candidate)
                .execute(&mut *db)
                .await?;
        }
        let previous = previous.map(|hash| hash.to_vec());
        sqlx::query!(
            "DELETE FROM sessions WHERE token_hash = $1 OR expires_at <= now()",
            previous,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, now() + $3 * interval '1 second')",
            session.as_slice(),
            id,
            f64::from(session_seconds),
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(User {
            id,
            name: name.to_owned(),
            avatar_url: avatar.to_owned(),
        })
    }

    pub async fn session_user(&self, hash: &[u8; 32]) -> Result<Option<User>, StoreError> {
        Ok(sqlx::query_as!(
            User,
            "SELECT u.id, u.name, u.avatar_url FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now() AND u.id <> $2",
            hash.as_slice(),
            DELETED_USER_ID,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn end_session(&self, hash: &[u8; 32]) -> Result<(), StoreError> {
        sqlx::query!(
            "DELETE FROM sessions WHERE token_hash = $1",
            hash.as_slice()
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn workbook_access(
        &self,
        doc: Uuid,
        user: Uuid,
    ) -> Result<Option<WorkbookAccess>, StoreError> {
        if user == DELETED_USER_ID {
            return Ok(None);
        }
        let row = sqlx::query!(
            "SELECT w.title, w.visibility, w.log_epoch, m.role,
                    COALESCE((SELECT MAX(seq) FROM txns WHERE workbook_id = w.id AND branch_id = $3), 0) AS \"head!\"
             FROM members m JOIN workbooks w ON w.id = m.workbook_id WHERE m.workbook_id = $1 AND m.user_id = $2",
            doc, user, MAIN_BRANCH,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(WorkbookAccess {
                id: doc,
                title: row.title,
                visibility: WorkbookVisibility::parse(&row.visibility)?,
                epoch: integer(row.log_epoch, "epoch")?,
                role: member_role(&row.role)?,
                head: integer(row.head, "head seq")?,
            })
        })
        .transpose()
    }

    pub async fn resolve_anonymous_share(
        &self,
        doc: Uuid,
        hash: &[u8; 32],
    ) -> Result<Option<AnonymousShareAccess>, StoreError> {
        let row = sqlx::query!(
            "SELECT s.id, w.title, w.log_epoch,
                    COALESCE((SELECT MAX(seq) FROM txns WHERE workbook_id = w.id AND branch_id = $3), 0) AS \"head!\"
             FROM share_links s
             JOIN workbooks w ON w.id = s.workbook_id
             WHERE s.workbook_id = $1
               AND s.token_hash = $2
               AND w.visibility = 'link_view'
               AND s.role = 'viewer'
               AND s.revoked_at IS NULL
               AND (s.expires_at IS NULL OR s.expires_at > now())
               AND (NOT s.single_use OR s.consumed_by IS NULL)",
            doc,
            hash.as_slice(),
            MAIN_BRANCH,
        )
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(AnonymousShareAccess {
                link_id: row.id,
                id: doc,
                title: row.title,
                epoch: integer(row.log_epoch, "epoch")?,
                head: integer(row.head, "head seq")?,
            })
        })
        .transpose()
    }

    pub async fn list_workbooks(&self, user: Uuid) -> Result<WorkbookLists, StoreError> {
        let rows = sqlx::query!(
            "SELECT w.id, w.title, w.visibility, m.role,
                    COALESCE((SELECT MAX(seq) FROM txns WHERE workbook_id = w.id AND branch_id = $2), 0) AS \"head_seq!\",
                    w.est_bytes, w.log_bytes,
                    to_char(w.updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS \"updated_at!\"
             FROM members m
             JOIN workbooks w ON w.id = m.workbook_id
             WHERE m.user_id = $1
             ORDER BY w.updated_at DESC, w.id ASC",
            user,
            MAIN_BRANCH,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut owned = Vec::new();
        let mut shared = Vec::new();
        for row in rows {
            let role_parsed = member_role(&row.role)?;
            let item = WorkbookListItem {
                id: row.id,
                branch_id: MAIN_BRANCH,
                title: row.title,
                role: row.role,
                visibility: WorkbookVisibility::parse(&row.visibility)?,
                head_seq: integer::<_, u64>(row.head_seq, "head seq")?.to_string(),
                est_bytes: integer(row.est_bytes, "est bytes")?,
                log_bytes: integer(row.log_bytes, "log bytes")?,
                updated_at: row.updated_at,
            };
            if role_parsed == MemberRole::Owner {
                owned.push(item);
            } else {
                shared.push(item);
            }
        }
        Ok(WorkbookLists { owned, shared })
    }

    pub async fn workbook_members(&self, doc: Uuid) -> Result<BTreeMap<String, u64>, StoreError> {
        let rows = sqlx::query!(
            "SELECT role, COUNT(*) AS \"count!\" FROM members WHERE workbook_id = $1 GROUP BY role",
            doc,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| Ok((row.role, integer(row.count, "member count")?)))
            .collect()
    }

    pub async fn list_members(&self, doc: Uuid) -> Result<Vec<MemberEntry>, StoreError> {
        let rows = sqlx::query!(
            "SELECT m.user_id, u.name, u.avatar_url, m.role
             FROM members m
             JOIN users u ON u.id = m.user_id
             WHERE m.workbook_id = $1
             ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'editor' THEN 1 ELSE 2 END, u.name ASC, m.user_id ASC",
            doc,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| MemberEntry {
                user_id: row.user_id,
                name: row.name,
                avatar_url: row.avatar_url,
                role: row.role,
            })
            .collect())
    }

    pub async fn update_member_role(
        &self,
        doc: Uuid,
        target_user: Uuid,
        role: MemberRole,
    ) -> Result<(), StoreError> {
        if target_user == DELETED_USER_ID {
            return Err(StoreError::InvalidInput(
                "cannot assign deleted user".to_owned(),
            ));
        }
        let role_str = match role {
            MemberRole::Editor => "editor",
            MemberRole::Viewer => "viewer",
            MemberRole::Owner => {
                return Err(StoreError::InvalidInput(
                    "only editor or viewer roles can be assigned".to_owned(),
                ));
            }
        };
        let mut db = self.pool.begin().await?;
        let current = sqlx::query!(
            "SELECT user_id, role FROM members WHERE workbook_id = $1 ORDER BY user_id FOR UPDATE",
            doc,
        )
        .fetch_all(&mut *db)
        .await?;
        let owner_count = current.iter().filter(|m| m.role == "owner").count();
        let Some(target) = current.iter().find(|m| m.user_id == target_user) else {
            return Err(StoreError::NotFound);
        };
        if target.role == "owner" && owner_count <= 1 {
            return Err(StoreError::InvalidInput(
                "cannot demote the sole workbook owner".to_owned(),
            ));
        }
        sqlx::query!(
            "UPDATE members SET role = $3 WHERE workbook_id = $1 AND user_id = $2",
            doc,
            target_user,
            role_str,
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(())
    }

    pub async fn remove_member(&self, doc: Uuid, target_user: Uuid) -> Result<(), StoreError> {
        let mut db = self.pool.begin().await?;
        let current = sqlx::query!(
            "SELECT user_id, role FROM members WHERE workbook_id = $1 ORDER BY user_id FOR UPDATE",
            doc,
        )
        .fetch_all(&mut *db)
        .await?;
        let owner_count = current.iter().filter(|m| m.role == "owner").count();
        let Some(target) = current.iter().find(|m| m.user_id == target_user) else {
            return Err(StoreError::NotFound);
        };
        if target.role == "owner" && owner_count <= 1 {
            return Err(StoreError::InvalidInput(
                "cannot remove the sole workbook owner".to_owned(),
            ));
        }
        sqlx::query!(
            "DELETE FROM members WHERE workbook_id = $1 AND user_id = $2",
            doc,
            target_user,
        )
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        Ok(())
    }

    /// The membership check and link creation share a statement boundary.
    pub async fn create_share_link(
        &self,
        doc: Uuid,
        author: Uuid,
        hash: &[u8; 32],
        single_use: bool,
        ttl: Option<u32>,
    ) -> Result<bool, StoreError> {
        Ok(self
            .create_share_link_with_options(
                doc,
                author,
                hash,
                MemberRole::Editor,
                "",
                single_use,
                ttl,
            )
            .await?
            .is_some())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "share link creation bundles role, label, single-use flag, and TTL atomically"
    )]
    pub async fn create_share_link_with_options(
        &self,
        doc: Uuid,
        author: Uuid,
        hash: &[u8; 32],
        role: MemberRole,
        label: &str,
        single_use: bool,
        ttl: Option<u32>,
    ) -> Result<Option<Uuid>, StoreError> {
        let role_str = match role {
            MemberRole::Editor => "editor",
            MemberRole::Viewer => "viewer",
            MemberRole::Owner => {
                return Err(StoreError::InvalidInput(
                    "share links can grant editor or viewer access".to_owned(),
                ));
            }
        };
        let clean_label = label.trim();
        if clean_label.chars().count() > 120 || clean_label.chars().any(char::is_control) {
            return Err(StoreError::InvalidInput(
                "share link label must be at most 120 printable characters".to_owned(),
            ));
        }
        let link_id = Uuid::new_v4();
        let inserted = sqlx::query_scalar!(
            "INSERT INTO share_links (token_hash, id, workbook_id, role, label, created_by, expires_at, single_use)
             SELECT $3, $4, $1, $5, $6, $2, now() + $7 * interval '1 second', $8
             FROM members WHERE workbook_id = $1 AND user_id = $2 AND role = 'owner'
             RETURNING id",
            doc,
            author,
            hash.as_slice(),
            link_id,
            role_str,
            clean_label,
            ttl.map(f64::from),
            single_use,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(inserted)
    }

    pub async fn list_share_links(&self, doc: Uuid) -> Result<Vec<ShareLinkRecord>, StoreError> {
        let rows = sqlx::query!(
            "SELECT id, role, label, single_use, (consumed_by IS NOT NULL) AS \"consumed!\",
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS \"created_at!\",
                    to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS expires_at,
                    to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS revoked_at
             FROM share_links
             WHERE workbook_id = $1
             ORDER BY created_at DESC",
            doc,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ShareLinkRecord {
                id: row.id,
                role: row.role,
                label: row.label,
                single_use: row.single_use,
                consumed: row.consumed,
                created_at: row.created_at,
                expires_at: row.expires_at,
                revoked_at: row.revoked_at,
            })
            .collect())
    }

    pub async fn revoke_share_link(&self, doc: Uuid, link_id: Uuid) -> Result<bool, StoreError> {
        let row = sqlx::query_scalar!(
            "UPDATE share_links SET revoked_at = COALESCE(revoked_at, now())
             WHERE workbook_id = $1 AND id = $2
             RETURNING id",
            doc,
            link_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn join_via_link(
        &self,
        user: Uuid,
        hash: &[u8; 32],
    ) -> Result<Option<Uuid>, StoreError> {
        if user == DELETED_USER_ID {
            return Err(StoreError::Forbidden);
        }
        let mut db = self.pool.begin().await?;
        let link = sqlx::query!(
            "SELECT workbook_id, role, single_use, consumed_by FROM share_links
             WHERE token_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > clock_timestamp())
             FOR UPDATE",
            hash.as_slice(),
        )
        .fetch_optional(&mut *db)
        .await?;
        let Some(link) = link else { return Ok(None) };
        if link.consumed_by.is_some_and(|previous| previous != user) {
            return Ok(None);
        }
        sqlx::query!(
            "INSERT INTO members (workbook_id, user_id, role) VALUES ($1, $2, $3)
             ON CONFLICT (workbook_id, user_id) DO UPDATE SET
               role = CASE
                 WHEN members.role = 'viewer' AND EXCLUDED.role = 'editor' THEN 'editor'
                 ELSE members.role
               END",
            link.workbook_id,
            user,
            link.role,
        )
        .execute(&mut *db)
        .await?;
        if link.single_use && link.consumed_by.is_none() {
            sqlx::query!(
                "UPDATE share_links SET consumed_by = $2 WHERE token_hash = $1",
                hash.as_slice(),
                user
            )
            .execute(&mut *db)
            .await?;
        }
        db.commit().await?;
        Ok(Some(link.workbook_id))
    }

    pub async fn account_summary(&self, user_id: Uuid) -> Result<AccountSummary, StoreError> {
        if user_id == DELETED_USER_ID {
            return Err(StoreError::NotFound);
        }
        let row = sqlx::query!(
            "SELECT u.id, u.name, u.avatar_url, u.storage_bytes,
                    (SELECT COUNT(*) FROM members m WHERE m.user_id = u.id AND m.role = 'owner') AS \"owned!\",
                    (SELECT COUNT(*) FROM members m WHERE m.user_id = u.id AND m.role <> 'owner') AS \"shared!\"
             FROM users u
             WHERE u.id = $1",
            user_id,
        )
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;

        Ok(AccountSummary {
            user: User {
                id: row.id,
                name: row.name,
                avatar_url: row.avatar_url,
            },
            owned_workbooks: integer(row.owned, "owned workbooks")?,
            shared_workbooks: integer(row.shared, "shared workbooks")?,
            storage_bytes: integer(row.storage_bytes, "storage bytes")?,
            quota_bytes: self.limits.user_quota_bytes,
            disk_over_soft_limit: self.disk_over_soft_limit(),
            disk_over_hard_limit: self.disk_over_hard_limit(),
        })
    }

    pub async fn delete_account(&self, user_id: Uuid) -> Result<DeleteAccountOutcome, StoreError> {
        if user_id == DELETED_USER_ID {
            return Err(StoreError::Forbidden);
        }
        let mut db = self.pool.begin().await?;
        sqlx::query!("SET CONSTRAINTS ALL DEFERRED")
            .execute(&mut *db)
            .await?;

        let exists = sqlx::query_scalar!("SELECT id FROM users WHERE id = $1 FOR UPDATE", user_id)
            .fetch_optional(&mut *db)
            .await?;
        if exists.is_none() {
            return Err(StoreError::NotFound);
        }

        let owned_workbooks = sqlx::query_scalar!(
            "SELECT id FROM workbooks WHERE owner_id = $1 ORDER BY id FOR UPDATE",
            user_id,
        )
        .fetch_all(&mut *db)
        .await?;

        let all_memberships = sqlx::query_scalar!(
            "SELECT workbook_id FROM members WHERE user_id = $1 ORDER BY workbook_id",
            user_id,
        )
        .fetch_all(&mut *db)
        .await?;
        let shared_workbooks: Vec<Uuid> = all_memberships
            .into_iter()
            .filter(|id| !owned_workbooks.contains(id))
            .collect();

        sqlx::query!("DELETE FROM workbooks WHERE owner_id = $1", user_id)
            .execute(&mut *db)
            .await?;

        sqlx::query!(
            "UPDATE replicas SET user_id = $2 WHERE user_id = $1",
            user_id,
            DELETED_USER_ID,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "UPDATE txns SET user_id = $2 WHERE user_id = $1",
            user_id,
            DELETED_USER_ID,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "UPDATE branches SET created_by = $2 WHERE created_by = $1",
            user_id,
            DELETED_USER_ID,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "UPDATE share_links SET created_by = $2 WHERE created_by = $1",
            user_id,
            DELETED_USER_ID,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "UPDATE share_links SET consumed_by = $2 WHERE consumed_by = $1",
            user_id,
            DELETED_USER_ID,
        )
        .execute(&mut *db)
        .await?;
        sqlx::query!(
            "UPDATE versions SET created_by = $2 WHERE created_by = $1",
            user_id,
            DELETED_USER_ID,
        )
        .execute(&mut *db)
        .await?;

        sqlx::query!("DELETE FROM users WHERE id = $1", user_id)
            .execute(&mut *db)
            .await?;
        db.commit().await?;

        for id in &owned_workbooks {
            let dir = self.data_dir.join("snapshots").join(id.to_string());
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        self.refresh_disk_used().await?;

        Ok(DeleteAccountOutcome {
            owned_workbooks,
            shared_workbooks,
        })
    }
}

fn member_role(role: &str) -> Result<MemberRole, StoreError> {
    match role {
        "owner" => Ok(MemberRole::Owner),
        "editor" => Ok(MemberRole::Editor),
        "viewer" => Ok(MemberRole::Viewer),
        _ => Err(StoreError::Invalid("member role")),
    }
}
