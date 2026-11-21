#!/usr/bin/env bash
set -euo pipefail

# Strict backup order: PostgreSQL dump first, then the `workbooks` volume so
# every snapshot row in Postgres references a file already on disk (or a later
# snapshot written during the backup window that `restore.sh` safely prunes).

COMPOSE_PROJECT="${COMPOSE_PROJECT:-reticula}"
COMPOSE_FILE="${COMPOSE_FILE:-infra/compose.yml}"
ENV_FILE="${ENV_FILE:-}"
BACKUP_ROOT="${BACKUP_ROOT:-/var/backups/reticula}"
KEEP_LOCAL_BACKUPS="${KEEP_LOCAL_BACKUPS:-7}"
POSTGRES_USER="${POSTGRES_USER:-reticula}"
POSTGRES_DB="${POSTGRES_DB:-reticula}"
WORKBOOKS_VOLUME="${WORKBOOKS_VOLUME:-${COMPOSE_PROJECT}_workbooks}"
UTIL_IMAGE="${UTIL_IMAGE:-postgres:18.7-bookworm}"

if [[ -n "$ENV_FILE" && -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  set +a
fi

compose() {
  local args=(compose --project-name "$COMPOSE_PROJECT" --file "$COMPOSE_FILE")
  if [[ -n "$ENV_FILE" ]]; then
    args+=(--env-file "$ENV_FILE")
  fi
  docker "${args[@]}" "$@"
}

mkdir -p "$BACKUP_ROOT"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TARGET_DIR="${BACKUP_ROOT}/${STAMP}"
STAGE_DIR="${TARGET_DIR}.tmp"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

cleanup() {
  rm -rf "$STAGE_DIR"
}
trap cleanup EXIT

echo "[backup] dumping PostgreSQL (${POSTGRES_DB})..."
compose exec -T postgres pg_dump \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --format=custom \
  --no-owner \
  --no-privileges \
  > "${STAGE_DIR}/reticula.pgdump"

echo "[backup] recording database table counts..."
compose exec -T postgres psql \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --set=ON_ERROR_STOP=1 \
  --tuples-only \
  --no-align \
  --command "
    SELECT 'users=' || count(*) FROM users;
    SELECT 'workbooks=' || count(*) FROM workbooks;
    SELECT 'members=' || count(*) FROM members;
    SELECT 'replicas=' || count(*) FROM replicas;
    SELECT 'branches=' || count(*) FROM branches;
    SELECT 'txns=' || count(*) FROM txns;
    SELECT 'snapshots=' || count(*) FROM snapshots;
    SELECT 'versions=' || count(*) FROM versions;
  " > "${STAGE_DIR}/row-counts.txt"

echo "[backup] archiving workbooks volume (${WORKBOOKS_VOLUME})..."
docker run --rm \
  -v "${WORKBOOKS_VOLUME}:/data:ro" \
  "$UTIL_IMAGE" \
  tar -czf - -C /data . \
  > "${STAGE_DIR}/workbooks.tar.gz"

echo "[backup] recording snapshot SHA-256 checksums..."
docker run --rm \
  -v "${WORKBOOKS_VOLUME}:/data:ro" \
  "$UTIL_IMAGE" \
  sh -c 'cd /data && find . -type f -name "*.rtclsnap" | LC_ALL=C sort | xargs -r sha256sum' \
  > "${STAGE_DIR}/snapshots.sha256"

(
  cd "$STAGE_DIR"
  sha256sum reticula.pgdump workbooks.tar.gz row-counts.txt snapshots.sha256 > manifest.sha256
)

chmod 600 "${STAGE_DIR}/reticula.pgdump" "${STAGE_DIR}/workbooks.tar.gz" \
  "${STAGE_DIR}/row-counts.txt" "${STAGE_DIR}/snapshots.sha256" "${STAGE_DIR}/manifest.sha256"

mv "$STAGE_DIR" "$TARGET_DIR"

if [[ "$KEEP_LOCAL_BACKUPS" =~ ^[0-9]+$ ]] && (( KEEP_LOCAL_BACKUPS > 0 )); then
  mapfile -t backups < <(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d ! -name "*.tmp" | LC_ALL=C sort)
  total="${#backups[@]}"
  if (( total > KEEP_LOCAL_BACKUPS )); then
    excess=$((total - KEEP_LOCAL_BACKUPS))
    for ((i = 0; i < excess; i++)); do
      echo "[backup] pruning old local backup ${backups[$i]}"
      rm -rf "${backups[$i]}"
    done
  fi
fi

echo "$TARGET_DIR"
