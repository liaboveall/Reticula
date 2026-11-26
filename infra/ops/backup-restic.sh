#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${ENV_FILE:-}"

if [[ -n "$ENV_FILE" && -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  set +a
fi

if [[ -z "${RESTIC_REPOSITORY:-}" ]]; then
  echo "error: RESTIC_REPOSITORY must be set" >&2
  exit 1
fi
if [[ -z "${RESTIC_PASSWORD:-}" && -z "${RESTIC_PASSWORD_FILE:-}" && -z "${RESTIC_PASSWORD_COMMAND:-}" ]]; then
  echo "error: RESTIC_PASSWORD, RESTIC_PASSWORD_FILE, or RESTIC_PASSWORD_COMMAND must be set" >&2
  exit 1
fi

BACKUP_DIR="${1:-}"
if [[ -z "$BACKUP_DIR" ]]; then
  BACKUP_DIR="$("${SCRIPT_DIR}/backup.sh" | tail -n 1)"
fi

if [[ ! -f "${BACKUP_DIR}/manifest.sha256" ]]; then
  echo "error: invalid backup directory: ${BACKUP_DIR}" >&2
  exit 1
fi

BACKUP_DIR="$(cd "$BACKUP_DIR" && pwd)"
# The tables the backup lists, which are not in it.
TABLE_BACKUP_ROOT="${TABLE_BACKUP_ROOT:-$(dirname "${BACKUP_DIR%/}")-tables}"
paths=("$BACKUP_DIR")
if [[ -d "$TABLE_BACKUP_ROOT" ]]; then
  paths+=("$TABLE_BACKUP_ROOT")
elif [[ -s "${BACKUP_DIR}/tables.tsv" ]]; then
  echo "error: ${BACKUP_DIR} lists tables and ${TABLE_BACKUP_ROOT} does not exist" >&2
  exit 1
fi

echo "[backup-restic] uploading ${paths[*]} to ${RESTIC_REPOSITORY}..."
if ! restic cat config >/dev/null 2>&1; then
  restic init
fi

restic backup --tag reticula "${paths[@]}"
# Every backup has a path of its own, and restic keeps the last of each path:
# what is kept is decided over all of them on this host.
restic forget \
  --tag reticula \
  --group-by host,tags \
  --keep-daily "${RESTIC_KEEP_DAILY:-7}" \
  --keep-weekly "${RESTIC_KEEP_WEEKLY:-4}" \
  --keep-monthly "${RESTIC_KEEP_MONTHLY:-6}" \
  --prune

echo "[backup-restic] complete: ${BACKUP_DIR}"
