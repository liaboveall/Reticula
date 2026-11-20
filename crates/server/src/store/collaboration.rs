//! Account, session and workbook access queries.

use std::collections::BTreeMap;

use reticula_sync::MemberRole;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{MAIN_BRANCH, Store, StoreError, integer};

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
    pub epoch: u8,
    pub role: MemberRole,
    pub head: u64,
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
        ).fetch_one(&mut *db).await?;
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
        sqlx::query!("INSERT INTO sessions (token_hash, user_id, expires_at) VALUES ($1, $2, now() + $3 * interval '1 second')",
            session.as_slice(), id, f64::from(session_seconds),
        ).execute(&mut *db).await?;
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
             WHERE s.token_hash = $1 AND s.expires_at > now()",
            hash.as_slice(),
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
        let row = sqlx::query!(
            "SELECT w.title, w.log_epoch, m.role,
                    COALESCE((SELECT MAX(seq) FROM txns WHERE workbook_id = w.id AND branch_id = $3), 0) AS \"head!\"
             FROM members m JOIN workbooks w ON w.id = m.workbook_id WHERE m.workbook_id = $1 AND m.user_id = $2",
            doc, user, MAIN_BRANCH,
        ).fetch_optional(&self.pool).await?;
        row.map(|row| {
            Ok(WorkbookAccess {
                id: doc,
                title: row.title,
                epoch: integer(row.log_epoch, "epoch")?,
                role: member_role(&row.role)?,
                head: integer(row.head, "head seq")?,
            })
        })
        .transpose()
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

    /// The membership check and link creation share a statement boundary.
    pub async fn create_share_link(
        &self,
        doc: Uuid,
        author: Uuid,
        hash: &[u8; 32],
        single_use: bool,
        ttl: Option<u32>,
    ) -> Result<bool, StoreError> {
        let changed = sqlx::query!(
            "INSERT INTO share_links (token_hash, workbook_id, role, created_by, expires_at, single_use)
             SELECT $3, $1, 'editor', $2, now() + $4 * interval '1 second', $5
             FROM members WHERE workbook_id = $1 AND user_id = $2 AND role = 'owner'",
            doc, author, hash.as_slice(), ttl.map(f64::from), single_use,
        ).execute(&self.pool).await?;
        Ok(changed.rows_affected() == 1)
    }

    pub async fn join_via_link(
        &self,
        user: Uuid,
        hash: &[u8; 32],
    ) -> Result<Option<Uuid>, StoreError> {
        let mut db = self.pool.begin().await?;
        let link = sqlx::query!(
            "SELECT workbook_id, single_use, consumed_by FROM share_links
             WHERE token_hash = $1 AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > clock_timestamp())
             FOR UPDATE",
            hash.as_slice(),
        ).fetch_optional(&mut *db).await?;
        let Some(link) = link else { return Ok(None) };
        if link.consumed_by.is_some_and(|previous| previous != user) {
            return Ok(None);
        }
        sqlx::query!(
            "INSERT INTO members (workbook_id, user_id, role) VALUES ($1, $2, 'editor') ON CONFLICT DO NOTHING",
            link.workbook_id, user,
        ).execute(&mut *db).await?;
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
}

fn member_role(role: &str) -> Result<MemberRole, StoreError> {
    match role {
        "owner" => Ok(MemberRole::Owner),
        "editor" => Ok(MemberRole::Editor),
        _ => Err(StoreError::Invalid("member role")),
    }
}
