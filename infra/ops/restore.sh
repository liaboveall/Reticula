#!/usr/bin/env bash
set -euo pipefail

# Puts a backup in place of what is there. The database is replaced in one
# transaction, or not at all; what is found to be wrong after that is reported,
# and the rest is done all the same.
#
# With --tables-only nothing but the files of tables is restored: those that are
# marked as missing and have turned up become ready. The server may be running.

usage() {
  echo "usage: $0 [--tables-only] <backup_dir>" >&2
  exit 1
}

TABLES_ONLY=0
if [[ "${1:-}" == "--tables-only" ]]; then
  TABLES_ONLY=1
  shift
fi
[[ $# -eq 1 ]] || usage

COMPOSE_PROJECT="${COMPOSE_PROJECT:-reticula}"
COMPOSE_FILE="${COMPOSE_FILE:-infra/compose.yml}"
ENV_FILE="${ENV_FILE:-}"
POSTGRES_USER="${POSTGRES_USER:-reticula}"
POSTGRES_DB="${POSTGRES_DB:-reticula}"
WORKBOOKS_VOLUME="${WORKBOOKS_VOLUME:-${COMPOSE_PROJECT}_workbooks}"
UTIL_IMAGE="${UTIL_IMAGE:-postgres:18.7-bookworm}"
START_SERVER="${START_SERVER:-1}"
WITHOUT_TABLES="${WITHOUT_TABLES:-0}"

if [[ -n "$ENV_FILE" && -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  set +a
fi

if [[ ! -f "${1}/manifest.sha256" ]]; then
  echo "error: missing ${1}/manifest.sha256" >&2
  exit 1
fi
# Docker takes a relative path for the name of a volume.
BACKUP_DIR="$(cd "$1" && pwd)"
# Where `backup.sh` puts it unless told otherwise, next to the backups, also
# when both have been brought back from somewhere else.
TABLE_BACKUP_ROOT="${TABLE_BACKUP_ROOT:-$(dirname "$BACKUP_DIR")-tables}"
if [[ "$TABLE_BACKUP_ROOT" != /* ]]; then
  TABLE_BACKUP_ROOT="${PWD}/${TABLE_BACKUP_ROOT}"
fi

compose() {
  local args=(compose --project-name "$COMPOSE_PROJECT" --file "$COMPOSE_FILE")
  if [[ -n "$ENV_FILE" ]]; then
    args+=(--env-file "$ENV_FILE")
  fi
  docker "${args[@]}" "$@"
}

run_psql() {
  compose exec -T postgres psql \
    --username="$POSTGRES_USER" \
    --dbname="$POSTGRES_DB" \
    --set=ON_ERROR_STOP=1 \
    --tuples-only \
    --no-align \
    --field-separator=$'\t' \
    "$@"
}

psql_rows() {
  run_psql --command "$1" < /dev/null
}

problems=()
problem() {
  echo "[restore] PROBLEM: $1" >&2
  problems+=("$1")
}

has_tables() {
  [[ "$(psql_rows "SELECT to_regclass('public.tables') IS NOT NULL" | tr -d '[:space:]')" == "t" ]]
}

# Puts the files of tables on the volume and says in the rows which are there.
restore_tables() {
  local store_mount=() rows found missing
  echo "[restore] restoring tables from ${TABLE_BACKUP_ROOT}..."
  if [[ -d "$TABLE_BACKUP_ROOT" ]]; then
    store_mount=(-v "${TABLE_BACKUP_ROOT}:/store:ro")
  fi
  rows="$(psql_rows "
    SELECT workbook_id, id, encode(sha256, 'hex'), bytes
    FROM tables WHERE status <> 'uploading' ORDER BY workbook_id, id
  ")" || return 1
  found="$(
    printf '%s\n' "$rows" | docker run --rm -i \
      -v "${WORKBOOKS_VOLUME}:/data" \
      "${store_mount[@]}" \
      "$UTIL_IMAGE" \
      sh -c '
        set -eu
        is_table() {
          [ -f "$1" ] && [ "$(stat -c %s "$1")" = "$3" ] &&
            [ "$(sha256sum < "$1" | cut -d" " -f1)" = "$2" ]
        }
        if [ -d /data/tables ]; then
          find /data/tables -type f -name "*.partial" -delete
        fi
        while IFS="$(printf "\t")" read -r workbook id sha bytes; do
          [ -n "$sha" ] || continue
          dir="/data/tables/$workbook"
          file="$dir/$id.rtbl"
          status=ready
          if ! is_table "$file" "$sha" "$bytes"; then
            source="/store/$(printf "%.2s" "$sha")/$sha.rtbl"
            status=missing
            if [ -f "$source" ]; then
              mkdir -p "$dir"
              chown 10001:10001 /data/tables "$dir"
              if cp "$source" "$file.partial" && is_table "$file.partial" "$sha" "$bytes"; then
                chown 10001:10001 "$file.partial"
                mv "$file.partial" "$file"
                status=ready
              else
                rm -f "$file.partial"
              fi
            fi
          fi
          printf "%s\t%s\t%s\n" "$workbook" "$id" "$status"
        done
      '
  )" || return 1
  missing="$(grep $'\tmissing$' <<< "$found" || true)"
  if [[ -n "$missing" ]]; then
    echo "[restore] WARNING: no copy of these tables is left; they are marked as missing:" >&2
    cut -f1,2 <<< "$missing" >&2
  fi
  {
    echo "BEGIN;"
    echo "CREATE TEMP TABLE found (workbook_id uuid, id uuid, status text);"
    echo "COPY found FROM STDIN;"
    if [[ -n "$found" ]]; then
      printf '%s\n' "$found"
    fi
    echo '\.'
    echo "WITH changed AS (
            UPDATE tables t SET status = f.status FROM found f
            WHERE t.workbook_id = f.workbook_id AND t.id = f.id
              AND t.status <> 'uploading' AND t.status <> f.status
            RETURNING t.workbook_id
          )
          UPDATE workbooks SET tables_version = tables_version + 1
          WHERE id IN (SELECT workbook_id FROM changed);"
    echo "COMMIT;"
  } | run_psql --quiet > /dev/null
}

echo "[restore] verifying archive checksums..."
(cd "$BACKUP_DIR" && sha256sum -c manifest.sha256)

# Going on would mark every one of them as missing.
if [[ -s "${BACKUP_DIR}/tables.tsv" && ! -d "$TABLE_BACKUP_ROOT" && "$WITHOUT_TABLES" != "1" ]]; then
  echo "error: ${BACKUP_DIR} lists tables, and ${TABLE_BACKUP_ROOT} does not exist; set TABLE_BACKUP_ROOT, or WITHOUT_TABLES=1 to go on without them" >&2
  exit 1
fi

if [[ "$TABLES_ONLY" == "1" ]]; then
  if ! has_tables; then
    echo "error: the database has no tables" >&2
    exit 1
  fi
  restore_tables
  echo "[restore] completed cleanly"
  exit 0
fi

echo "[restore] stopping application server and ensuring PostgreSQL is ready..."
was_running="$(compose ps --status running --quiet server 2> /dev/null || true)"
compose up --no-start
compose up -d --wait postgres
compose stop server > /dev/null 2>&1 || true

# Until the database has been replaced, a failure leaves everything as it was.
replaced=0
on_exit() {
  local code=$?
  if (( ! replaced )); then
    docker run --rm -v "${WORKBOOKS_VOLUME}:/data" "$UTIL_IMAGE" rm -rf /data/restore.staged < /dev/null || true
    echo "[restore] failed; nothing has been changed" >&2
    if [[ -n "$was_running" ]]; then
      compose start server || true
    fi
  fi
  exit "$code"
}
trap on_exit EXIT

echo "[restore] unpacking workbooks archive..."
docker run --rm -i \
  -v "${WORKBOOKS_VOLUME}:/data" \
  "$UTIL_IMAGE" \
  sh -c 'rm -rf /data/restore.staged && mkdir /data/restore.staged && tar -xzf - -C /data/restore.staged' \
  < "${BACKUP_DIR}/workbooks.tar.gz"

# The schema goes as a whole, with relations the backup does not know of. Every
# workbook gets an epoch that it has not had, here or in the backup, so that
# clients from before can tell. Without the last line nothing of it happens.
echo "[restore] restoring PostgreSQL dump..."
{
  echo "
    BEGIN;
    CREATE TEMP TABLE epochs_before (id uuid, log_epoch smallint);
    DO \$\$ BEGIN
      IF to_regclass('public.workbooks') IS NOT NULL THEN
        INSERT INTO epochs_before SELECT id, log_epoch FROM public.workbooks;
      END IF;
    END \$\$;
    SET client_min_messages = warning;
    DROP SCHEMA IF EXISTS public CASCADE;
    CREATE SCHEMA public AUTHORIZATION pg_database_owner;
    GRANT USAGE ON SCHEMA public TO PUBLIC;"
  compose exec -T postgres pg_restore \
    --no-owner \
    --no-privileges \
    --file=- \
    < "${BACKUP_DIR}/reticula.pgdump"
  echo "
    DO \$\$ BEGIN
      IF EXISTS (
        SELECT 1 FROM public.workbooks w LEFT JOIN pg_temp.epochs_before e USING (id)
        WHERE GREATEST(w.log_epoch, e.log_epoch) >= 255
      ) THEN
        RAISE EXCEPTION 'a workbook is at the last log_epoch and cannot have another';
      END IF;
    END \$\$;
    UPDATE public.workbooks w
    SET log_epoch = GREATEST(
          w.log_epoch,
          COALESCE((SELECT e.log_epoch FROM pg_temp.epochs_before e WHERE e.id = w.id), 0)
        ) + 1,
        next_per_epoch_replica = 1,
        updated_at = now();
    COMMIT;"
} | run_psql --quiet > /dev/null
replaced=1

# The tables on the volume stay: those the backup has need no copying, and the
# server puts the others aside when it starts.
echo "[restore] restoring workbooks volume (${WORKBOOKS_VOLUME})..."
docker run --rm \
  -v "${WORKBOOKS_VOLUME}:/data" \
  "$UTIL_IMAGE" \
  sh -c '
    set -eu
    cd /data
    find . -mindepth 1 -maxdepth 1 ! -name tables ! -name tables-removed ! -name restore.staged -exec rm -rf {} +
    find restore.staged -mindepth 1 -maxdepth 1 -exec mv -t . {} +
    rmdir restore.staged
    chown -R 10001:10001 .
  ' < /dev/null || problem "the workbooks volume could not be restored"

if [[ -f "${BACKUP_DIR}/row-counts.txt" ]]; then
  echo "[restore] verifying table row counts..."
  # Those the backup has: an older one knows of fewer relations.
  counts_sql=""
  while IFS='=' read -r relation _; do
    if [[ "$relation" =~ ^[a-z_]+$ ]]; then
      counts_sql+="SELECT '${relation}=' || count(*) FROM ${relation};"
    fi
  done < "${BACKUP_DIR}/row-counts.txt"
  actual_counts="$(psql_rows "$counts_sql" || true)"
  if ! diff -u "${BACKUP_DIR}/row-counts.txt" <(printf '%s\n' "$actual_counts"); then
    echo "[restore] WARNING: the row counts are not those of the backup" >&2
  fi
fi

echo "[restore] verifying snapshot checksums and detecting backup-window omissions..."
snapshot_rows="$(psql_rows "
  SELECT workbook_id, branch_id, seq, storage_key, encode(sha256, 'hex')
  FROM snapshots
  ORDER BY workbook_id, seq;
")"
snapshot_findings="$(
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
          [ "$actual" != "$expected" ] || continue
          finding=mismatch
        elif [ "$seq" = "0" ]; then
          finding=base
        else
          finding=prune
        fi
        printf "%s\t%s\t%s\t%s\t%s\n" "$finding" "$wb" "$branch" "$seq" "$key"
      done
    '
)"

while IFS=$'\t' read -r finding wb branch seq key; do
  case "$finding" in
    mismatch)
      problem "checksum mismatch of ${key}"
      ;;
    base)
      problem "first snapshot missing: ${key}"
      ;;
    prune)
      echo "[restore] pruning missing backup-window snapshot metadata (${key})..."
      psql_rows "
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
      " > /dev/null || problem "could not forget ${key}"
      ;;
  esac
done <<< "$snapshot_findings"

# A backup from before tables has no relation, and the server makes one.
if has_tables; then
  restore_tables || problem "the tables could not be restored; try again with --tables-only"
fi

if (( ${#problems[@]} > 0 )); then
  echo "[restore] everything else was restored, and the server was not started:" >&2
  printf '  %s\n' "${problems[@]}" >&2
  exit 1
fi

if [[ "$START_SERVER" == "1" ]]; then
  echo "[restore] starting server..."
  compose up -d server
fi

echo "[restore] completed cleanly"
