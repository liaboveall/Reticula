# Reticula Operations Runbook

## Architecture on the Host

`infra/compose.yml` runs two containers with explicit resource ceilings:

- `postgres`: `postgres:18.7-bookworm` (`768m` RAM, `0.5` CPU, `shared_buffers=128MB`, `max_connections=30`), storing its data directory in the `pgdata` Docker volume.
- `server`: `reticula-server` (`768m` RAM, `1.0` CPU, `read_only: true`), serving:
  - HTTP + WebSocket on `127.0.0.1:${PORT:-3200}` (fronted by host Nginx TLS per `infra/nginx/reticula.conf.example`);
  - Loopback Prometheus metrics on `127.0.0.1:${METRICS_PORT:-3201}/metrics` via `RETICULA_METRICS_ADDR`.
- `workbooks` Docker volume (`/var/lib/reticula`, owned by UID/GID `10001:10001`): stores `snapshots/<workbook_id>/<seq>.rtclsnap`.

Keep secrets and domain settings in a root-owned `.env` file (mode `0600`) outside the repository:

```sh
POSTGRES_PASSWORD=<strong-random-secret>
RETICULA_PUBLIC_ORIGIN=https://example.com
RETICULA_GITHUB_CLIENT_ID=<github-oauth-client-id>
RETICULA_GITHUB_CLIENT_SECRET=<github-oauth-secret>
```

## Backup Schedule

Strict order: dump PostgreSQL first, then tar the `workbooks` volume. Any `seq > 0` snapshot row written in the brief interval between `pg_dump` and `tar` is handled safely on restore without losing committed transactions.

1. Dry-run a local backup:

```sh
ENV_FILE=/etc/reticula/reticula.env BACKUP_ROOT=/var/backups/reticula ./infra/ops/backup.sh
```

2. Off-site encrypted backup with `restic`:

```sh
export RESTIC_REPOSITORY=s3:https://...
export RESTIC_PASSWORD_FILE=/etc/reticula/restic.pass
ENV_FILE=/etc/reticula/reticula.env ./infra/ops/backup-restic.sh
```

Suggested cron schedule (daily at `03:15` UTC, monitor every 5 minutes):

```cron
15 3 * * * ENV_FILE=/etc/reticula/reticula.env BACKUP_ROOT=/var/backups/reticula /opt/reticula/infra/ops/backup-restic.sh >> /var/log/reticula-backup.log 2>&1
*/5 * * * * ENV_FILE=/etc/reticula/reticula.env BACKUP_ROOT=/var/backups/reticula /opt/reticula/infra/ops/monitor.sh >> /var/log/reticula-monitor.log 2>&1
```

## Restore & Migration to a Fresh Host

1. Provision Docker, Nginx (`infra/nginx/reticula.conf.example`), `/etc/reticula/reticula.env`, and retrieve the target backup directory (directly from `/var/backups/reticula/<stamp>` or via `restic restore latest --target /tmp/restore`).
2. Run the restore script:

```sh
ENV_FILE=/etc/reticula/reticula.env ./infra/ops/restore.sh /var/backups/reticula/<stamp>
```

`restore.sh` automatically:
1. Verifies `manifest.sha256`.
2. Restores `reticula.pgdump` into PostgreSQL and extracts `workbooks.tar.gz` into the `workbooks` volume with `10001:10001` ownership.
3. Compares table row counts against `row-counts.txt` and validates every snapshot file's `SHA-256` against PostgreSQL.
4. Aborts if any `seq = 0` base snapshot is missing or any file checksum mismatches; prunes dangling `seq > 0` snapshot rows caused by the live backup window (the server replays those transactions from `txns` on next open).
5. Verifies `log_epoch < 255` and runs `UPDATE workbooks SET log_epoch = log_epoch + 1, next_per_epoch_replica = 1, updated_at = now();` so returning offline clients cannot collide on row/column IDs or confirmed sequence numbers.

Verify locally in Docker at any time with:

```sh
cargo xtask drill
```

## Incident Triage

- **503 / 4008 ServerBusy or high container memory**: inspect `http://127.0.0.1:3201/metrics` (`reticula_active_rooms`, `reticula_loaded_bytes`, `reticula_active_ws_connections`) and container logs (`docker compose logs --tail=200 server`). Idle rooms evict automatically when loaded bytes exceed `RETICULA_MAX_LOADED_BYTES` (default `384 MiB`).
- **Disk soft threshold (`80%`) / 507 Insufficient Storage**: check `reticula_disk_used_bytes` and `reticula_disk_over_soft_limit`, confirm snapshot pruning (`reticula_snapshots_pruned_total`), and remove obsolete off-box/local backups if the host disk is filling.
- **Slow commits**: inspect structured JSON logs for `slow batch flush` warnings (`> 50 ms`) and check PostgreSQL `pg_stat_activity`.
