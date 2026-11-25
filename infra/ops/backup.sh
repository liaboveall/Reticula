#!/usr/bin/env bash
set -euo pipefail

# The dump, the row counts and the list of tables are of one snapshot of the
# database. The `workbooks` volume comes after it, so every snapshot row in the
# dump references a file already on disk, or one that went during the backup,
# whose row `restore.sh` forgets.
#
# Tables are large and never change, so they are not in the archive of each day.
# They are kept once each, under their SHA-256, in TABLE_BACKUP_ROOT, for as long
# as a backup that is kept lists them in its `tables.tsv`.

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

# Docker takes a relative path for the name of a volume.
absolute() {
  if [[ "$1" == /* ]]; then printf '%s\n' "${1%/}"; else printf '%s\n' "${PWD}/${1%/}"; fi
}
BACKUP_ROOT="$(absolute "$BACKUP_ROOT")"
TABLE_BACKUP_ROOT="$(absolute "${TABLE_BACKUP_ROOT:-${BACKUP_ROOT}-tables}")"
TABLE_BACKUP_MAX_BYTES="${TABLE_BACKUP_MAX_BYTES:-12884901888}"
if [[ "${TABLE_BACKUP_ROOT}/" == "${BACKUP_ROOT}/"* ]]; then
  echo "error: TABLE_BACKUP_ROOT must be outside BACKUP_ROOT" >&2
  exit 1
fi

compose() {
  local args=(compose --project-name "$COMPOSE_PROJECT" --file "$COMPOSE_FILE")
  if [[ -n "$ENV_FILE" ]]; then
    args+=(--env-file "$ENV_FILE")
  fi
  docker "${args[@]}" "$@"
}

# Removes the tables that no backup lists.
collect_tables() {
  local listed
  listed="$(find "$BACKUP_ROOT" -mindepth 2 -maxdepth 2 -name tables.tsv -exec cut -f3 {} + | sort -u)"
  while IFS= read -r file; do
    if ! grep -qxF "$(basename "$file" .rtbl)" <<< "$listed"; then
      echo "[backup] removing table that no backup lists: ${file}"
      rm -f "$file"
    fi
  done < <(find "$TABLE_BACKUP_ROOT" -mindepth 2 -maxdepth 2 -type f -name '*.rtbl')
  find "$TABLE_BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d -empty -delete
}

mkdir -p "$BACKUP_ROOT" "$TABLE_BACKUP_ROOT"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TARGET_DIR="${BACKUP_ROOT}/${STAMP}"
STAGE_DIR="${TARGET_DIR}.tmp"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

cleanup() {
  rm -rf "$STAGE_DIR"
}
trap cleanup EXIT

# A session that holds the snapshot for as long as it is open.
coproc SNAPSHOT {
  compose exec -T postgres psql \
    --username="$POSTGRES_USER" \
    --dbname="$POSTGRES_DB" \
    --set=ON_ERROR_STOP=1 \
    --quiet \
    --tuples-only \
    --no-align \
    --field-separator=$'\t'
}
SNAPSHOT_SESSION="$SNAPSHOT_PID"
exec {to_session}>&"${SNAPSHOT[1]}" {from_session}<&"${SNAPSHOT[0]}"

# Answers a query from that session.
ask() {
  local line
  printf '%s\n\\echo --end--\n' "$1" >&"$to_session"
  while IFS= read -r line <&"$from_session"; do
    if [[ "$line" == "--end--" ]]; then
      return 0
    fi
    printf '%s\n' "$line"
  done
  echo "error: lost the database session of the backup" >&2
  return 1
}

ask "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;"
snapshot="$(ask "SELECT pg_export_snapshot();")"

echo "[backup] recording row counts and tables..."
relations=(users workbooks members replicas branches txns snapshots versions agent_runs agent_steps quotas)
# A server from before tables has no such relation.
if [[ "$(ask "SELECT to_regclass('public.tables') IS NOT NULL;")" == "t" ]]; then
  relations+=(tables)
  ask "SELECT workbook_id, id, encode(sha256, 'hex'), bytes, status
       FROM tables WHERE status <> 'uploading' ORDER BY workbook_id, id;" > "${STAGE_DIR}/tables.tsv"
else
  : > "${STAGE_DIR}/tables.tsv"
fi
for relation in "${relations[@]}"; do
  ask "SELECT '${relation}=' || count(*) FROM ${relation};"
done > "${STAGE_DIR}/row-counts.txt"

echo "[backup] dumping PostgreSQL (${POSTGRES_DB})..."
compose exec -T postgres pg_dump \
  --username="$POSTGRES_USER" \
  --dbname="$POSTGRES_DB" \
  --snapshot="$snapshot" \
  --format=custom \
  --no-owner \
  --no-privileges \
  < /dev/null > "${STAGE_DIR}/reticula.pgdump"

ask "COMMIT;"
exec {to_session}>&- {from_session}<&- {SNAPSHOT[1]}>&-
wait "$SNAPSHOT_SESSION"

# First, to make room.
collect_tables

# A table that was deleted since the dump is missed, and will be missing after a
# restore. One that is missing already has its copy here, or none.
echo "[backup] copying tables to ${TABLE_BACKUP_ROOT}..."
docker run --rm -i \
  -v "${WORKBOOKS_VOLUME}:/data:ro" \
  -v "${TABLE_BACKUP_ROOT}:/store" \
  -e LIMIT="$TABLE_BACKUP_MAX_BYTES" \
  -e OWNER="$(id -u):$(id -g)" \
  "$UTIL_IMAGE" \
  sh -c '
    set -eu
    umask 077
    find /store -type f -name "*.partial" -delete
    used="$(find /store -type f -printf "%s\n" | awk "{ sum += \$1 } END { printf \"%.0f\", sum }")"
    while IFS="$(printf "\t")" read -r workbook id sha bytes status; do
      [ -n "$sha" ] || continue
      dir="/store/$(printf "%.2s" "$sha")"
      file="$dir/$sha.rtbl"
      if [ -f "$file" ] && [ "$(stat -c %s "$file")" = "$bytes" ]; then
        continue
      fi
      source="/data/tables/$workbook/$id.rtbl"
      reason=""
      if [ ! -f "$source" ]; then
        [ "$status" != missing ] || continue
        reason="not on the volume"
      elif [ "$((used + bytes))" -gt "$LIMIT" ]; then
        reason="over the limit of the table backups"
      else
        mkdir -p "$dir"
        cp "$source" "$file.partial"
        if [ "$(sha256sum < "$file.partial" | cut -d" " -f1)" = "$sha" ]; then
          chown "$OWNER" "$dir" "$file.partial"
          mv "$file.partial" "$file"
          used="$((used + bytes))"
        else
          rm -f "$file.partial"
          rmdir "$dir" 2>/dev/null || true
          reason="not the bytes that were uploaded"
        fi
      fi
      if [ -n "$reason" ]; then
        printf "%s\t%s\t%s\t%s\t%s\n" "$workbook" "$id" "$sha" "$bytes" "$reason"
      fi
    done
  ' \
  < "${STAGE_DIR}/tables.tsv" > "${STAGE_DIR}/tables-missed.tsv"
if [[ -s "${STAGE_DIR}/tables-missed.tsv" ]]; then
  echo "[backup] WARNING: $(wc -l < "${STAGE_DIR}/tables-missed.tsv") table(s) are not backed up:" >&2
  cat "${STAGE_DIR}/tables-missed.tsv" >&2
fi

echo "[backup] archiving workbooks volume (${WORKBOOKS_VOLUME})..."
docker run --rm \
  -v "${WORKBOOKS_VOLUME}:/data:ro" \
  "$UTIL_IMAGE" \
  tar -czf - -C /data --exclude=./tables --exclude=./tables-removed . \
  > "${STAGE_DIR}/workbooks.tar.gz"

echo "[backup] recording snapshot SHA-256 checksums..."
docker run --rm \
  -v "${WORKBOOKS_VOLUME}:/data:ro" \
  "$UTIL_IMAGE" \
  sh -c 'cd /data && find . -type f -name "*.rtclsnap" | LC_ALL=C sort | xargs -r sha256sum' \
  > "${STAGE_DIR}/snapshots.sha256"

(
  cd "$STAGE_DIR"
  sha256sum reticula.pgdump workbooks.tar.gz row-counts.txt snapshots.sha256 \
    tables.tsv tables-missed.tsv > manifest.sha256
)

chmod 600 "${STAGE_DIR}/reticula.pgdump" "${STAGE_DIR}/workbooks.tar.gz" \
  "${STAGE_DIR}/row-counts.txt" "${STAGE_DIR}/snapshots.sha256" \
  "${STAGE_DIR}/tables.tsv" "${STAGE_DIR}/tables-missed.tsv" "${STAGE_DIR}/manifest.sha256"

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
    collect_tables
  fi
fi

echo "$TARGET_DIR"
