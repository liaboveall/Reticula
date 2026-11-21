#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <backup_dir>" >&2
  exit 1
fi

BACKUP_DIR="$1"
COMPOSE_PROJECT="${COMPOSE_PROJECT:-reticula}"
COMPOSE_FILE="${COMPOSE_FILE:-infra/compose.yml}"
ENV_FILE="${ENV_FILE:-}"
POSTGRES_USER="${POSTGRES_USER:-reticula}"
POSTGRES_DB="${POSTGRES_DB:-reticula}"
WORKBOOKS_VOLUME="${WORKBOOKS_VOLUME:-${COMPOSE_PROJECT}_workbooks}"
UTIL_IMAGE="${UTIL_IMAGE:-postgres:18.7-bookworm}"
START_SERVER="${START_SERVER:-1}"

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

if [[ ! -f "${BACKUP_DIR}/manifest.sha256" ]]; then
  echo "error: missing ${BACKUP_DIR}/manifest.sha256" >&2
  exit 1
fi

echo "[restore] verifying archive checksums..."
(cd "$BACKUP_DIR" && sha256sum -c manifest.sha256)

echo "[restore] stopping application server and ensuring PostgreSQL is ready..."
compose up --no-start
compose up -d --wait postgres
compose stop server >/dev/null 2>&1 || true

echo "[restore] restoring PostgreSQL dump..."
compose exec -T postgres pg_restore \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --clean \
  --if-exists \
  --no-owner \
  --no-privileges \
  --exit-on-error \
  < "${BACKUP_DIR}/reticula.pgdump"

echo "[restore] restoring workbooks volume (${WORKBOOKS_VOLUME})..."
docker run --rm -i \
  -v "${WORKBOOKS_VOLUME}:/data" \
  "$UTIL_IMAGE" \
  sh -c 'find /data -mindepth 1 -delete && tar -xzf - -C /data && chown -R 10001:10001 /data' \
  < "${BACKUP_DIR}/workbooks.tar.gz"

if [[ -f "${BACKUP_DIR}/row-counts.txt" ]]; then
  echo "[restore] verifying table row counts..."
  actual_counts="$(compose exec -T postgres psql \
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
    ")"
  diff -u "${BACKUP_DIR}/row-counts.txt" <(printf '%s\n' "$actual_counts")
fi

echo "[restore] verifying snapshot checksums and detecting backup-window omissions..."
snapshot_rows="$(compose exec -T postgres psql \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --set=ON_ERROR_STOP=1 \
  --tuples-only \
  --no-align \
  --field-separator=$'\t' \
  --command "
    SELECT workbook_id, branch_id, seq, storage_key, encode(sha256, 'hex')
    FROM snapshots
    ORDER BY workbook_id, seq;
  ")"

missing_rows="$(
  printf '%s\n' "$snapshot_rows" | docker run --rm -i \
    -v "${WORKBOOKS_VOLUME}:/data:ro" \
    "$UTIL_IMAGE" \
    sh -c '
      set -eu
      while IFS="$(printf "\t")" read -r wb branch seq key expected; do
        [ -n "$wb" ] || continue
        file="/data/$key"
        if [ -f "$file" ]; then
          actual="$(sha256sum "$file" | cut -d" " -f1)"
          if [ "$actual" != "$expected" ]; then
            echo "checksum mismatch for $key: expected $expected got $actual" >&2
            exit 1
          fi
        else
          if [ "$seq" = "0" ]; then
            echo "fatal: base seq=0 snapshot missing: $key" >&2
            exit 1
          fi
          printf "%s\t%s\t%s\t%s\n" "$wb" "$branch" "$seq" "$key"
        fi
      done
    '
)"

if [[ -n "$missing_rows" ]]; then
  while IFS=$'\t' read -r wb branch seq key; do
    [[ -n "$wb" ]] || continue
    echo "[restore] pruning missing backup-window snapshot metadata (${key})..."
    compose exec -T postgres psql \
      --username="$POSTGRES_USER" \
      --dbname="$POSTGRES_DB" \
      --set=ON_ERROR_STOP=1 \
      --command "
        BEGIN;
        WITH removed AS (
          DELETE FROM snapshots
          WHERE workbook_id = '${wb}'::uuid
            AND branch_id = '${branch}'::uuid
            AND seq = ${seq}
          RETURNING workbook_id, size_bytes
        )
        UPDATE users u
        SET storage_bytes = GREATEST(0, u.storage_bytes - r.size_bytes)
        FROM workbooks w
        JOIN removed r ON r.workbook_id = w.id
        WHERE u.id = w.owner_id;
        COMMIT;
      " >/dev/null
  done <<< "$missing_rows"
fi

echo "[restore] checking epoch headrooms and incrementing log_epoch..."
saturated="$(compose exec -T postgres psql \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --set=ON_ERROR_STOP=1 \
  --tuples-only \
  --no-align \
  --command "SELECT count(*) FROM workbooks WHERE log_epoch >= 255;" | tr -d '[:space:]')"
if [[ "$saturated" != "0" ]]; then
  echo "error: ${saturated} workbook(s) already at log_epoch >= 255; cannot increment" >&2
  exit 1
fi

compose exec -T postgres psql \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --set=ON_ERROR_STOP=1 \
  --command "
    UPDATE workbooks
    SET log_epoch = log_epoch + 1,
        next_per_epoch_replica = 1,
        updated_at = now();
  " >/dev/null

if [[ "$START_SERVER" == "1" ]]; then
  echo "[restore] starting server..."
  compose up -d server
fi

echo "[restore] completed cleanly"
