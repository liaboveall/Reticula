#!/usr/bin/env bash
set -euo pipefail

RETICULA_URL="${RETICULA_URL:-http://127.0.0.1:3200}"
METRICS_URL="${METRICS_URL:-http://127.0.0.1:3201/metrics}"
COMPOSE_PROJECT="${COMPOSE_PROJECT:-reticula}"
COMPOSE_FILE="${COMPOSE_FILE:-infra/compose.yml}"
ENV_FILE="${ENV_FILE:-}"
BACKUP_ROOT="${BACKUP_ROOT:-/var/backups/reticula}"
DATA_MOUNT="${DATA_MOUNT:-/}"
DISK_WARN_PCT="${DISK_WARN_PCT:-80}"
MEM_WARN_PCT="${MEM_WARN_PCT:-80}"
BACKUP_MAX_AGE_HOURS="${BACKUP_MAX_AGE_HOURS:-26}"
ALERT_WEBHOOK_URL="${ALERT_WEBHOOK_URL:-}"

if [[ -n "$ENV_FILE" && -f "$ENV_FILE" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  set +a
fi

TABLE_BACKUP_ROOT="${TABLE_BACKUP_ROOT:-${BACKUP_ROOT%/}-tables}"
TABLE_BACKUP_MAX_BYTES="${TABLE_BACKUP_MAX_BYTES:-12884901888}"
TABLE_BACKUP_WARN_PCT="${TABLE_BACKUP_WARN_PCT:-80}"

failures=()
alert() {
  local msg="$1"
  echo "[monitor] ALERT: $msg" >&2
  failures+=("$msg")
}

# What has been found goes out however the script ends.
finished=0
report() {
  local code=$?
  trap - EXIT
  if (( ! finished )); then
    alert "the monitor stopped before its end (exit ${code})"
  fi
  if (( ${#failures[@]} > 0 )); then
    if [[ -n "$ALERT_WEBHOOK_URL" ]]; then
      local summary
      summary="$(printf '%s; ' "${failures[@]}")"
      curl -fsS -X POST -H "Content-Type: application/json" \
        -d "{\"text\":\"Reticula monitor alert: ${summary}\"}" \
        "$ALERT_WEBHOOK_URL" >/dev/null || true
    fi
    exit 1
  fi
}
trap report EXIT

check_http() {
  local url="$1"
  if ! curl -fsS --max-time 5 "$url" >/dev/null; then
    alert "endpoint check failed: $url"
  fi
}

check_http "${RETICULA_URL}/healthz"
check_http "${RETICULA_URL}/readyz"

if metrics="$(curl -fsS --max-time 5 "$METRICS_URL" 2>/dev/null)"; then
  over_soft="$(printf '%s\n' "$metrics" | awk '/^reticula_disk_over_soft_limit / { print $2; exit }')"
  if [[ "$over_soft" == "1" ]]; then
    alert "reticula_disk_over_soft_limit=1 on ${METRICS_URL}"
  fi
else
  alert "could not scrape ${METRICS_URL}"
fi

disk_pct="$(df -P "$DATA_MOUNT" | awk 'NR==2 { gsub("%", "", $5); print $5 }' || true)"
if [[ -z "$disk_pct" ]]; then
  alert "could not read the filesystem usage on ${DATA_MOUNT}"
elif (( disk_pct >= DISK_WARN_PCT )); then
  alert "filesystem usage on ${DATA_MOUNT} is ${disk_pct}% (>= ${DISK_WARN_PCT}%)"
fi

latest_backup=""
if [[ ! -d "$BACKUP_ROOT" ]]; then
  # No directory is no backup, not one that is fine.
  alert "the backup directory ${BACKUP_ROOT} does not exist"
else
  latest_backup="$(find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d ! -name "*.tmp" | LC_ALL=C sort | tail -n 1 || true)"
  if [[ -z "$latest_backup" ]]; then
    alert "no completed backups found under ${BACKUP_ROOT}"
  else
    manifest="${latest_backup}/manifest.sha256"
    if [[ ! -f "$manifest" ]]; then
      alert "latest backup ${latest_backup} is missing manifest.sha256"
    else
      now_epoch="$(date -u +%s)"
      # A backup that is being pruned is as old as can be.
      mtime_epoch="$(stat -c %Y "$manifest" 2>/dev/null || stat -f %m "$manifest" 2>/dev/null || echo 0)"
      age_hours="$(( (now_epoch - mtime_epoch) / 3600 ))"
      if (( age_hours > BACKUP_MAX_AGE_HOURS )); then
        alert "latest backup ${latest_backup} is ${age_hours}h old (> ${BACKUP_MAX_AGE_HOURS}h)"
      fi
    fi
    missed="${latest_backup}/tables-missed.tsv"
    if [[ -s "$missed" ]]; then
      alert "latest backup ${latest_backup} is without $(wc -l < "$missed" | tr -d ' ') table(s)"
    fi
  fi
fi

if [[ ! -d "$TABLE_BACKUP_ROOT" ]]; then
  if [[ -n "$latest_backup" && -s "${latest_backup}/tables.tsv" ]]; then
    alert "the table backup directory ${TABLE_BACKUP_ROOT} does not exist, and ${latest_backup} has tables"
  fi
else
  # A file may go while it is looked at.
  table_bytes="$({ find "$TABLE_BACKUP_ROOT" -type f -printf '%s\n' 2>/dev/null || true; } | awk '{ sum += $1 } END { printf "%.0f", sum }')"
  if (( table_bytes * 100 >= TABLE_BACKUP_MAX_BYTES * TABLE_BACKUP_WARN_PCT )); then
    alert "table backups in ${TABLE_BACKUP_ROOT} take ${table_bytes} of ${TABLE_BACKUP_MAX_BYTES} bytes (>= ${TABLE_BACKUP_WARN_PCT}%)"
  fi
fi

if command -v docker >/dev/null 2>&1; then
  compose_args=(compose --project-name "$COMPOSE_PROJECT" --file "$COMPOSE_FILE")
  if [[ -n "$ENV_FILE" ]]; then
    compose_args+=(--env-file "$ENV_FILE")
  fi
  if container_ids="$(docker "${compose_args[@]}" ps --quiet 2>/dev/null)" && [[ -n "$container_ids" ]]; then
    while IFS=$'\t' read -r cname mem_pct; do
      pct_int="${mem_pct%%.*}"
      pct_int="${pct_int//[^0-9]/}"
      if [[ -n "$pct_int" ]] && (( pct_int >= MEM_WARN_PCT )); then
        alert "container ${cname} memory usage is ${mem_pct} (>= ${MEM_WARN_PCT}%)"
      fi
    done < <(docker stats --no-stream --format "{{.Name}}\t{{.MemPerc}}" $container_ids)
  fi
fi

finished=1
if (( ${#failures[@]} == 0 )); then
  echo "[monitor] ok (disk=${disk_pct:-?}%)"
fi
