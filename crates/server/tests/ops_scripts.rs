//! The scripts in `infra/ops`: what a backup has, what a restore does with it,
//! and what the monitor says. They run against a PostgreSQL container and a
//! volume of their own; the server has no part in it.

use std::path::Path;
use std::process::{Command, Output};

/// What every test starts with: a directory to work in, a database with the
/// schema, a workbook, and two tables with their files on the volume.
const PRELUDE: &str = r##"
set -uo pipefail
WORK="$(mktemp -d)"
cd "$WORK"
failed=0
check() {
  if [[ "$2" == "$3" ]]; then
    echo "ok: $1"
  else
    printf 'NOT OK: %s\n  got:  %s\n  want: %s\n' "$1" "$2" "$3"
    failed=1
  fi
}
has() {
  if grep -qF -- "$2" "$3"; then echo "ok: $1"; else echo "NOT OK: $1: no '$2' in"; cat "$3"; failed=1; fi
}
"##;

const DATABASE: &str = r##"
IMAGE=postgres:18.7-bookworm
export COMPOSE_PROJECT="reticula-ops-$$-$RANDOM" COMPOSE_FILE="$WORK/compose.yml" START_SERVER=0
DOCKER="$(command -v docker)"
cat > compose.yml <<EOF
services:
  postgres:
    image: $IMAGE
    environment: { POSTGRES_USER: reticula, POSTGRES_PASSWORD: x, POSTGRES_DB: reticula }
    volumes: ["pgdata:/var/lib/postgresql"]
    healthcheck: { test: ["CMD-SHELL", "pg_isready -U reticula -d reticula -h 127.0.0.1"], interval: 1s, timeout: 3s, retries: 60 }
    mem_limit: 256m
  server:
    image: $IMAGE
    entrypoint: ["sleep", "3600"]
    volumes: ["workbooks:/var/lib/reticula"]
volumes: { pgdata: {}, workbooks: {} }
EOF
project() { "$DOCKER" compose --project-name "$COMPOSE_PROJECT" --file "$COMPOSE_FILE" "$@"; }
sql() { project exec -T postgres psql -U reticula -d reticula -q -At -v ON_ERROR_STOP=1 -c "$1" < /dev/null; }
volume() { "$DOCKER" run --rm -i -v "${COMPOSE_PROJECT}_workbooks:/data" "$IMAGE" sh -c "$1"; }
cleanup() {
  project down --volumes > /dev/null 2>&1
  "$DOCKER" run --rm -v "$WORK:/work" "$IMAGE" sh -c 'rm -rf /work/*' > /dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT
project up -d --wait postgres > up.log 2>&1 && project up --no-start >> up.log 2>&1 || { cat up.log; exit 1; }
for migration in "$MIGRATIONS"/*.sql; do
  project exec -T postgres psql -U reticula -d reticula -q -v ON_ERROR_STOP=1 < "$migration" || exit 1
done
BOOK=00000000-0000-0000-0000-0000000000b1
USER_=00000000-0000-0000-0000-0000000000aa
sql "INSERT INTO users (id, name, avatar_url) VALUES ('$USER_', 'A', '');
     INSERT INTO workbooks (id, owner_id, title) VALUES ('$BOOK', '$USER_', 'Before');"
declare_table() { # number, status
  sql "INSERT INTO tables (workbook_id, id, name, position, bytes, sha256, status, created_by)
       VALUES ('$BOOK', '00000000-0000-0000-0000-00000000000$1', 't$1', $1, $(stat -c %s "$WORK/file$1"),
               decode('$(sha256sum < "$WORK/file$1" | cut -d' ' -f1)', 'hex'), '$2', '$USER_')"
}
for n in 1 2; do
  head -c $((n * 100000)) /dev/urandom > "file$n"
  declare_table $n ready
  volume "mkdir -p /data/tables/$BOOK && cat > /data/tables/$BOOK/00000000-0000-0000-0000-00000000000$n.rtbl" < "file$n"
done
statuses() { sql "SELECT status FROM tables ORDER BY id" | tr '\n' ' '; }
backup() { bash "$OPS/backup.sh" > backup.log 2>&1; code=$?; BACKUP="$(tail -n 1 backup.log)"; return $code; }
restore() { bash "$OPS/restore.sh" "$@" > "$WORK/restore.log" 2>&1; }
"##;

fn is_available(command: &mut Command) -> bool {
    command
        .output()
        .is_ok_and(|output: Output| output.status.success())
}

/// A shell with Docker: on Windows that of WSL.
fn shell() -> Option<Vec<String>> {
    if cfg!(windows) {
        let found = is_available(Command::new("wsl").args(["-e", "docker", "version"]));
        found.then(|| ["wsl", "-e", "bash"].map(str::to_owned).to_vec())
    } else {
        assert!(
            is_available(Command::new("docker").arg("version")),
            "Docker is not running"
        );
        Some(vec!["bash".to_owned()])
    }
}

/// A path as that shell has it.
fn shell_path(path: &Path) -> String {
    let path = path
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    match path.trim_start_matches("//?/").split_once(":/") {
        Some((drive, rest)) => format!("/mnt/{}/{rest}", drive.to_lowercase()),
        None => path,
    }
}

fn run(parts: &[&str]) {
    let Some(shell) = shell() else {
        eprintln!("skipped: there is no WSL distribution with Docker");
        return;
    };
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = format!(
        "OPS='{}'\nMIGRATIONS='{}'\n{PRELUDE}{}\nexit \"$failed\"\n",
        shell_path(&crate_dir.join("../../infra/ops")),
        shell_path(&crate_dir.join("migrations")),
        parts.concat(),
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.sh");
    std::fs::write(&file, script).unwrap();
    let output = Command::new(&shell[0])
        .args(&shell[1..])
        .arg(shell_path(&file))
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{text}");
    assert!(output.status.success(), "{text}");
}

#[test]
#[ignore = "requires bash and Docker"]
fn a_backup_taken_while_something_is_written() {
    run(&[
        DATABASE,
        r##"
# Writes as soon as the dump is done, before anything else is asked for.
mkdir shim
cat > shim/docker <<'EOF'
#!/usr/bin/env bash
"$DOCKER" "$@"
code=$?
if [[ "$*" == *pg_dump* ]]; then
  "$DOCKER" compose --project-name "$COMPOSE_PROJECT" --file "$COMPOSE_FILE" exec -T postgres \
    psql -U reticula -d reticula -q -v ON_ERROR_STOP=1 -c "$AFTER_DUMP" < /dev/null >&2
fi
exit $code
EOF
chmod +x shim/docker
export DOCKER BACKUP_ROOT="$WORK/bk"
export AFTER_DUMP="INSERT INTO users (id, name, avatar_url) VALUES (gen_random_uuid(), 'B', '');
  DELETE FROM tables WHERE id = '00000000-0000-0000-0000-000000000002';
  INSERT INTO workbooks (id, owner_id, title) VALUES (gen_random_uuid(), '$USER_', 'During')"
PATH="$WORK/shim:$PATH" backup
check "a backup is made" $? 0
check "it has the rows of the dump" "$(grep -E '^(users|workbooks|tables)=' "$BACKUP/row-counts.txt" | tr '\n' ' ')" "users=2 workbooks=1 tables=2 "
check "and lists its tables" "$(wc -l < "$BACKUP/tables.tsv")" 2
restore "$BACKUP"
check "it can be restored" $? 0
check "without a difference" "$(grep -c WARNING restore.log)" 0
check "as it was" "$(sql 'SELECT title FROM workbooks')" Before
check "in another epoch" "$(sql 'SELECT log_epoch FROM workbooks')" 1

# One that was made when the rows were counted after the dump.
sql "UPDATE workbooks SET title = 'After'"
sed -i 's/^users=.*/users=7/' "$BACKUP/row-counts.txt"
(cd "$BACKUP" && sha256sum reticula.pgdump workbooks.tar.gz row-counts.txt snapshots.sha256 > manifest.sha256)
restore "$BACKUP"
check "other counts do not stop a restore" $? 0
has "they are reported" "WARNING" restore.log
check "it went on to the end" "$(sql 'SELECT title, log_epoch FROM workbooks')" "Before|2"
check "with the tables" "$(statuses)" "ready ready "
"##,
    ]);
}

#[test]
#[ignore = "requires bash and Docker"]
fn the_tables_of_a_backup() {
    run(&[
        DATABASE,
        r##"
# By a relative path, and not into a volume of that name.
export BACKUP_ROOT=bk
backup
check "a backup under a relative path" $? 0
check "has the copies there" "$(find bk-tables -type f -name '*.rtbl' | wc -l)" 2
check "and in no volume" "$("$DOCKER" volume ls -q | grep -cx bk-tables)" 0
check "the archive has no tables" "$(tar -tzf "$BACKUP/workbooks.tar.gz" | grep -c tables)" 0
BACKUP_ROOT="$WORK/bk"

# On a fresh host, by the name of the directory alone.
volume "rm -rf /data/tables"
(cd bk && restore "$(basename "$BACKUP")")
check "a restore by a relative path" $? 0
check "finds the copies" "$(statuses)" "ready ready "
check "and puts them back" "$(volume "cat /data/tables/$BOOK/*2.rtbl" < /dev/null | sha256sum)" "$(sha256sum < file2)"

# A table whose file is lost keeps its copy for as long as it is in a backup.
volume "rm /data/tables/$BOOK/*2.rtbl"
sql "UPDATE tables SET status = 'missing' WHERE position = 2"
for _ in 1 2; do sleep 1.1; KEEP_LOCAL_BACKUPS=1 backup; done
check "copies left" "$(find bk-tables -type f -name '*.rtbl' | wc -l)" 2
check "backups left" "$(ls bk | wc -l)" 1
check "nothing is said to be without a backup" "$(wc -c < "$BACKUP/tables-missed.tsv")" 0

# The tables alone, without anything else going back.
sql "UPDATE workbooks SET title = 'Edited since'"
before="$(sql 'SELECT tables_version FROM workbooks')"
epoch="$(sql 'SELECT log_epoch FROM workbooks')"
restore --tables-only "$BACKUP"
check "the tables alone are restored" $? 0
check "and ready" "$(statuses)" "ready ready "
check "nothing else changed" "$(sql 'SELECT title, log_epoch FROM workbooks')" "Edited since|$epoch"
check "the list did" "$([[ "$(sql 'SELECT tables_version FROM workbooks')" -gt "$before" ]] && echo yes)" yes
before="$(sql 'SELECT tables_version FROM workbooks')"
restore --tables-only "$BACKUP"
check "again is nothing" "$? $(sql 'SELECT tables_version FROM workbooks')" "0 $before"
"##,
    ]);
}

#[test]
#[ignore = "requires bash and Docker"]
fn what_a_restore_changes() {
    run(&[
        DATABASE,
        r##"
export BACKUP_ROOT="$WORK/bk"
backup
sql "UPDATE workbooks SET title = 'Edited since'"

# Without the copies nothing is touched, unless that is asked for.
mv bk-tables elsewhere
volume "rm /data/tables/$BOOK/*2.rtbl"
restore "$BACKUP"
check "a restore without the copies is refused" $? 1
check "before anything changed" "$(sql 'SELECT title FROM workbooks')" "Edited since"
restore --tables-only "$BACKUP"
check "so is one of the tables alone" $? 1
WITHOUT_TABLES=1 restore "$BACKUP"
check "it goes on when told to" "$? $(sql 'SELECT title FROM workbooks') $(statuses)" "0 Before ready missing "
mv elsewhere bk-tables
restore --tables-only "$BACKUP"
check "the copies turned up" "$? $(statuses)" "0 ready ready "

# Every restore is an epoch of its own.
sql "UPDATE workbooks SET log_epoch = 41"
restore "$BACKUP"
check "above that of the database" "$(sql 'SELECT log_epoch FROM workbooks')" 42
restore "$BACKUP"
check "and once more" "$(sql 'SELECT log_epoch FROM workbooks')" 43
sql "UPDATE workbooks SET log_epoch = 255, title = 'Edited again'"
restore "$BACKUP"
check "no epoch is left" "$(($? != 0))" 1
has "which is said" "last log_epoch" restore.log
check "and nothing changed" "$(sql 'SELECT title, log_epoch FROM workbooks')" "Edited again|255"
check "nor is anything left over" "$(volume 'ls /data' < /dev/null | tr '\n' ' ')" "tables "

# A copy that is not what it should be.
sql "UPDATE workbooks SET log_epoch = 0"
volume "rm /data/tables/$BOOK/*2.rtbl"
for copy in bk-tables/*/"$(sha256sum < file2 | cut -d' ' -f1).rtbl"; do printf x | dd of="$copy" conv=notrunc 2> /dev/null; done
restore "$BACKUP"
check "a copy with other bytes" "$? $(statuses)" "0 ready missing "
has "it is reported" "missing" restore.log
"##,
    ]);
}

#[test]
#[ignore = "requires bash and Docker"]
fn snapshots_that_are_not_in_the_backup() {
    run(&[
        DATABASE,
        r##"
export BACKUP_ROOT="$WORK/bk"
sql "INSERT INTO branches (id, workbook_id, created_by) VALUES ('00000000-0000-0000-0000-000000000000', '$BOOK', '$USER_')"
snapshot() { # seq
  sql "INSERT INTO snapshots (workbook_id, branch_id, seq, format_version, storage_key, size_bytes, sha256)
       VALUES ('$BOOK', '00000000-0000-0000-0000-000000000000', $1, 1, 'snapshots/$BOOK/$1.rtclsnap', 10, decode('$(printf x | sha256sum | cut -d' ' -f1)', 'hex'))"
}
for seq in 5 6 7; do snapshot $seq; done
backup
restore "$BACKUP"
check "missing ones do not stop a restore" $? 0
check "all of them are forgotten" "$(sql 'SELECT COUNT(*) FROM snapshots')" 0

# One that has to be there, or is not what it was: everything else is restored,
# and the server is left to whoever looks into it.
snapshot 0
snapshot 8
printf x | volume "mkdir -p /data/snapshots/$BOOK && cat > /data/snapshots/$BOOK/8.rtclsnap"
sleep 1.1
backup
sql "UPDATE workbooks SET title = 'After', log_epoch = 3"
restore "$BACKUP"
check "a first snapshot that is missing" $? 1
check "everything else was done" "$(sql 'SELECT title, log_epoch FROM workbooks') $(statuses)" "Before|4 ready ready "
has "the problem is named" "snapshots/$BOOK/0.rtclsnap" restore.log
"##,
    ]);
}

#[test]
#[ignore = "requires bash and Docker"]
fn a_backup_from_before_tables() {
    run(&[
        DATABASE,
        r##"
export BACKUP_ROOT="$WORK/bk"
sql "DROP TABLE tables; ALTER TABLE users DROP COLUMN table_bytes; ALTER TABLE workbooks DROP COLUMN tables_version"
backup
check "a backup of a server without tables" $? 0
check "lists none" "$(wc -c < "$BACKUP/tables.tsv")" 0
# As the script of the time left it.
(cd "$BACKUP" && rm tables.tsv tables-missed.tsv && sha256sum reticula.pgdump workbooks.tar.gz row-counts.txt snapshots.sha256 > manifest.sha256)
rm -rf bk-tables
sql "UPDATE workbooks SET title = 'After'"
restore "$BACKUP"
check "can be restored" $? 0
check "as it was" "$(sql 'SELECT title, log_epoch FROM workbooks')" "Before|1"
restore --tables-only "$BACKUP"
check "there are no tables to restore alone" $? 1

# Into a database that has them by now.
project exec -T postgres psql -U reticula -d reticula -q -v ON_ERROR_STOP=1 < "$MIGRATIONS/0005_m5_tables.sql"
declare_table 1 ready
restore "$BACKUP"
check "can be restored over a newer one" "$?" 0
check "of which nothing is left" "$(sql "SELECT to_regclass('public.tables') IS NULL, log_epoch FROM workbooks")" "t|2"
"##,
    ]);
}

#[test]
#[ignore = "requires bash and Docker"]
fn what_the_monitor_says() {
    run(&[r##"
trap 'rm -rf "$WORK"' EXIT
mkdir -p shim bk/20260101T000000Z bk-tables/ab
touch bk/20260101T000000Z/manifest.sha256
for n in 1 2 3; do head -c 1000 /dev/zero > bk-tables/ab/$n.rtbl; done
# Nothing is running but the hook, which writes down what it is told.
cat > shim/curl <<EOF
#!/usr/bin/env bash
[[ "\$*" == *-X\ POST* ]] && echo "\$*" >> "$WORK/hook.log"
[[ "\$*" == *metrics* ]] && echo "reticula_disk_over_soft_limit 0"
exit 0
EOF
printf '#!/usr/bin/env bash\nexit 1\n' > shim/docker
chmod +x shim/*
export BACKUP_ROOT="$WORK/bk" DISK_WARN_PCT=101 ALERT_WEBHOOK_URL=http://127.0.0.1:9/hook
monitor() { rm -f hook.log; touch hook.log; PATH="$WORK/shim:$PATH" bash "$OPS/monitor.sh" > monitor.log 2>&1; }
monitor
check "nothing to report" "$? $(wc -l < hook.log)" "0 0"
TABLE_BACKUP_MAX_BYTES=3700 monitor
check "copies near their limit" $? 1
has "are reported" "3000 of 3700 bytes" hook.log
TABLE_BACKUP_MAX_BYTES=3800 monitor
check "not so near" $? 0
printf 'a\nb\n' > bk/20260101T000000Z/tables-missed.tsv
monitor
check "tables without a backup" $? 1
has "are reported" "without 2 table" hook.log

# Files that go while they are looked at, as they do during a backup.
real="$(command -v find)"
printf '#!/usr/bin/env bash\n%s "$@"\necho "find: No such file or directory" >&2\nexit 1\n' "$real" > shim/find
chmod +x shim/find
monitor
check "what was found so far" $? 1
has "is reported all the same" "without 2 table" hook.log
rm shim/find

# Whatever else goes wrong.
DATA_MOUNT="$WORK/none" monitor
check "a mount that is not there" $? 1
has "what is known is reported" "without 2 table" hook.log
"##]);
}

#[test]
#[ignore = "requires bash and Docker"]
fn what_goes_off_the_host() {
    run(&[r##"
trap 'rm -rf "$WORK"' EXIT
mkdir -p shim bk/20260101T000000Z bk-tables/ab
printf '#!/usr/bin/env bash\necho "$*" >> "%s/restic.log"\n' "$WORK" > shim/restic
chmod +x shim/restic
export PATH="$WORK/shim:$PATH" RESTIC_REPOSITORY=/r RESTIC_PASSWORD=x
touch bk/20260101T000000Z/manifest.sha256
printf 'a\tb\tc\t1\tready\n' > bk/20260101T000000Z/tables.tsv
(cd bk && bash "$OPS/backup-restic.sh" 20260101T000000Z > "$WORK/out.log" 2>&1)
check "a backup is uploaded" $? 0
has "with the copies of its tables" "backup --tag reticula $WORK/bk/20260101T000000Z $WORK/bk-tables" restic.log
rm -rf bk-tables restic.log
bash "$OPS/backup-restic.sh" "$WORK/bk/20260101T000000Z" > out.log 2>&1
check "not without those it lists" "$? $([[ -e restic.log ]] && echo uploaded)" "1 "
"##]);
}
