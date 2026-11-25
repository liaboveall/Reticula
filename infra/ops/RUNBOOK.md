# Reticula Operations Runbook

## Architecture on the Host

`infra/compose.yml` runs two containers with explicit resource ceilings:

- `postgres`: `postgres:18.7-bookworm` (`768m` RAM, `0.5` CPU, `shared_buffers=128MB`, `max_connections=30`), storing its data directory in the `pgdata` Docker volume.
- `server`: `reticula-server` (`768m` RAM, `1.0` CPU, `read_only: true`), serving:
  - HTTP + WebSocket on `127.0.0.1:${PORT:-3200}` (fronted by host Nginx TLS per `infra/nginx/reticula.conf.example`);
  - Loopback Prometheus metrics on `127.0.0.1:${METRICS_PORT:-3201}/metrics` via `RETICULA_METRICS_ADDR`.
- `workbooks` Docker volume (`/var/lib/reticula`, owned by UID/GID `10001:10001`): stores `snapshots/<workbook_id>/<seq>.rtclsnap` and the tables of workbooks, `tables/<workbook_id>/<table_id>.rtbl` (`.part` while one is being uploaded).

Nginx names the visitor in `X-Real-IP`, which the server believes from the peers in `RETICULA_TRUSTED_PROXIES` only. `infra/compose.yml` sets it to the networks Docker takes addresses from. Set it when the daemon has other address pools; otherwise all anonymous visitors share one limit. The server does not start when it cannot read the value.

Keep secrets and domain settings in a root-owned `.env` file (mode `0600`) outside the repository:

```sh
POSTGRES_PASSWORD=<strong-random-secret>
RETICULA_PUBLIC_ORIGIN=https://example.com
RETICULA_GITHUB_CLIENT_ID=<github-oauth-client-id>
RETICULA_GITHUB_CLIENT_SECRET=<github-oauth-secret>
```

## Backup Schedule

The dump, the row counts and the list of tables are of one snapshot of the database, so whatever is written during a backup is not in it. The archive of the `workbooks` volume comes after the dump. Any `seq > 0` snapshot file pruned in between is handled safely on restore without losing committed transactions.

The files of tables are not in the archive of each day. `backup.sh` keeps one copy of each under its SHA-256 in `TABLE_BACKUP_ROOT` (default: `BACKUP_ROOT` with `-tables` after it), and lists in `tables.tsv` which of them a backup needs, those that are `missing` included. A copy goes when no backup in `BACKUP_ROOT` lists it any more. Backups that are kept elsewhere need a copy of the directory as well; `backup-restic.sh` uploads both. Copies stop at `TABLE_BACKUP_MAX_BYTES` (default 12 GiB): the backup goes on without the tables that do not fit, names them in `tables-missed.tsv`, and `monitor.sh` reports it, as it does when `TABLE_BACKUP_WARN_PCT` (default 80) percent are in use.

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
1. Verifies `manifest.sha256`, and refuses to go on when the backup lists tables and `TABLE_BACKUP_ROOT` (default: the directory of the backups with `-tables` after it) does not exist. `WITHOUT_TABLES=1` restores the rest all the same.
2. Stops the server and unpacks `workbooks.tar.gz` next to what is on the volume.
3. Replaces the database with `reticula.pgdump` in one transaction, which also gives every workbook a `log_epoch` one above the highest it has had, in the database that was there or in the backup, and sets `next_per_epoch_replica = 1`. Returning offline clients therefore cannot collide on row/column IDs or confirmed sequence numbers, and a second restore is told apart from the first. A workbook at `log_epoch = 255` fails the transaction. Up to here a failure leaves everything as it was, and the server is started again if it was running.
4. Puts the unpacked archive in place, with `10001:10001` ownership.
5. Compares row counts against `row-counts.txt` (a difference is reported and does not stop anything), validates every snapshot file's `SHA-256` against PostgreSQL, and prunes dangling `seq > 0` snapshot rows caused by the live backup window (the server replays those transactions from `txns` on next open).
6. Copies the tables that are not on the volume from `TABLE_BACKUP_ROOT`, checking the SHA-256 of each. Tables that are in neither are marked `missing`: they stay in the list of the workbook, cannot be read, and count towards the owner's quota until they are deleted. A backup from before tables is restored without this step.
7. Starts the server. When a snapshot file does not match, or a `seq = 0` snapshot is missing, it names them and exits with an error instead, with everything else restored; `docker compose up -d server` starts it without them.

When the copy of a missing table has turned up, put it into `TABLE_BACKUP_ROOT` and restore the tables alone. Nothing else is touched, and the server may be running:

```sh
ENV_FILE=/etc/reticula/reticula.env ./infra/ops/restore.sh --tables-only /var/backups/reticula/<stamp>
```

Files of tables on the volume that are in no row of the restored database are put aside by the server when it starts, into `tables-removed/<seconds>/<workbook_id>/`, and removed after seven days. A file whose row comes back is put back. When most of the files are in no row the server leaves them where they are and says so in its log: the database may be the wrong one. Otherwise move them away by hand.

Verify locally in Docker at any time with:

```sh
cargo xtask drill
```

## Incident Triage

- **503 / 4008 ServerBusy or high container memory**: inspect `http://127.0.0.1:3201/metrics` (`reticula_active_rooms`, `reticula_loaded_bytes`, `reticula_active_ws_connections`) and container logs (`docker compose logs --tail=200 server`). Idle rooms evict automatically when loaded bytes exceed `RETICULA_MAX_LOADED_BYTES` (default `384 MiB`).
- **Disk soft threshold (`80%`) / 507 Insufficient Storage**: check `reticula_disk_used_bytes` and `reticula_disk_over_soft_limit`, confirm snapshot pruning (`reticula_snapshots_pruned_total`), and remove obsolete off-box/local backups if the host disk is filling. Tables count from the moment they are declared (`RETICULA_TABLE_*`), and uploads are refused below `RETICULA_MIN_FREE_BYTES` (default 1 GiB) on the volume; an upload that gets nothing for an hour is dropped.
- **Slow commits**: inspect structured JSON logs for `slow batch flush` warnings (`> 50 ms`) and check PostgreSQL `pg_stat_activity`.
