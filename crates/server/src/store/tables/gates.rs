//! Who may touch the file of a table, one at a time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock,
    watch,
};
use uuid::Uuid;

/// Gates nobody is at are dropped when there are this many.
const PRUNE_AT: usize = 1_024;

/// A lock for each table, and one for all tables of a workbook.
///
/// Whoever writes the file of a table holds its lock and shares the lock of
/// the workbook, which deleting the workbook takes for itself. A deletion
/// announces itself first, so that an upload that is slow to arrive does not
/// hold it up.
#[derive(Default)]
pub(super) struct TableGates {
    workbooks: Mutex<HashMap<Uuid, Weak<WorkbookGate>>>,
}

#[derive(Default)]
struct WorkbookGate {
    closing: watch::Sender<usize>,
    access: Arc<RwLock<()>>,
    tables: Mutex<HashMap<Uuid, Weak<TableGate>>>,
}

#[derive(Default)]
struct TableGate {
    closing: watch::Sender<usize>,
    access: Arc<AsyncMutex<()>>,
}

/// The right to write the file of a table.
pub(super) struct TableGuard {
    // The locks go first, while the gates can still be found.
    _table: OwnedMutexGuard<()>,
    _workbook: OwnedRwLockReadGuard<()>,
    table: Arc<TableGate>,
    workbook: Arc<WorkbookGate>,
}

/// The right to delete a table.
pub(super) struct ClosedTable {
    _guard: TableGuard,
    _closing: Closing,
}

/// The right to delete all tables of a workbook.
pub(in crate::store) struct ClosedWorkbook {
    _all: OwnedRwLockWriteGuard<()>,
    _closing: Closing,
    _gate: Arc<WorkbookGate>,
}

/// An announced deletion, until it is over.
struct Closing(watch::Sender<usize>);

impl Closing {
    fn announce(count: &watch::Sender<usize>) -> Self {
        count.send_modify(|count| *count += 1);
        Self(count.clone())
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count = count.saturating_sub(1));
    }
}

impl TableGuard {
    /// Whether the table is about to be deleted.
    pub(super) fn is_closing(&self) -> bool {
        *self.table.closing.borrow() > 0 || *self.workbook.closing.borrow() > 0
    }

    /// Ends when the table is about to be deleted.
    pub(super) async fn closing(&self) {
        let mut table = self.table.closing.subscribe();
        let mut workbook = self.workbook.closing.subscribe();
        tokio::select! {
            _ = table.wait_for(|count| *count > 0) => {}
            _ = workbook.wait_for(|count| *count > 0) => {}
        }
    }
}

impl TableGates {
    pub(super) async fn enter(&self, workbook: Uuid, table: Uuid) -> TableGuard {
        let workbook = self.workbook(workbook);
        let shared = Arc::clone(&workbook.access).read_owned().await;
        let table = workbook.table(table);
        let own = Arc::clone(&table.access).lock_owned().await;
        TableGuard {
            _table: own,
            _workbook: shared,
            table,
            workbook,
        }
    }

    /// Enters unless someone is there already.
    pub(super) fn try_enter(&self, workbook: Uuid, table: Uuid) -> Option<TableGuard> {
        let workbook = self.workbook(workbook);
        let shared = Arc::clone(&workbook.access).try_read_owned().ok()?;
        let table = workbook.table(table);
        let own = Arc::clone(&table.access).try_lock_owned().ok()?;
        Some(TableGuard {
            _table: own,
            _workbook: shared,
            table,
            workbook,
        })
    }

    pub(super) async fn close_table(&self, workbook: Uuid, table: Uuid) -> ClosedTable {
        // Held on to, so that those who enter find the same gates.
        let book = self.workbook(workbook);
        let gate = book.table(table);
        let closing = Closing::announce(&gate.closing);
        ClosedTable {
            _guard: self.enter(workbook, table).await,
            _closing: closing,
        }
    }

    pub(super) async fn close_workbook(&self, workbook: Uuid) -> ClosedWorkbook {
        let gate = self.workbook(workbook);
        let closing = Closing::announce(&gate.closing);
        ClosedWorkbook {
            _all: Arc::clone(&gate.access).write_owned().await,
            _closing: closing,
            _gate: gate,
        }
    }

    fn workbook(&self, id: Uuid) -> Arc<WorkbookGate> {
        find(locked(&self.workbooks), id)
    }
}

impl WorkbookGate {
    fn table(&self, id: Uuid) -> Arc<TableGate> {
        find(locked(&self.tables), id)
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn find<T: Default>(mut gates: MutexGuard<'_, HashMap<Uuid, Weak<T>>>, id: Uuid) -> Arc<T> {
    if let Some(gate) = gates.get(&id).and_then(Weak::upgrade) {
        return gate;
    }
    if gates.len() >= PRUNE_AT {
        gates.retain(|_, gate| gate.strong_count() > 0);
    }
    let gate = Arc::new(T::default());
    gates.insert(id, Arc::downgrade(&gate));
    gate
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    const BOOK: Uuid = Uuid::from_u128(1);
    const TABLE: Uuid = Uuid::from_u128(2);
    const OTHER: Uuid = Uuid::from_u128(3);
    const SOON: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn one_at_a_time_for_each_table() {
        let gates = TableGates::default();
        let first = gates.enter(BOOK, TABLE).await;
        assert!(timeout(SOON, gates.enter(BOOK, TABLE)).await.is_err());
        assert!(gates.try_enter(BOOK, TABLE).is_none());
        let _other = gates.enter(BOOK, OTHER).await;
        let _elsewhere = gates.enter(OTHER, TABLE).await;
        drop(first);
        timeout(SOON, gates.enter(BOOK, TABLE)).await.unwrap();
    }

    #[tokio::test]
    async fn a_deletion_is_announced_to_whoever_is_in_and_whoever_waits() {
        let gates = Arc::new(TableGates::default());
        let first = gates.enter(BOOK, TABLE).await;
        assert!(!first.is_closing());
        assert!(timeout(SOON, first.closing()).await.is_err());
        let waiting = tokio::spawn({
            let gates = Arc::clone(&gates);
            async move { gates.enter(BOOK, TABLE).await.is_closing() }
        });
        tokio::task::yield_now().await;
        let deletion = tokio::spawn({
            let gates = Arc::clone(&gates);
            async move { gates.close_table(BOOK, TABLE).await }
        });
        timeout(SOON, first.closing()).await.unwrap();
        assert!(first.is_closing());
        drop(first);
        assert!(waiting.await.unwrap());
        drop(deletion.await.unwrap());
        assert!(!gates.enter(BOOK, TABLE).await.is_closing());
    }

    #[tokio::test]
    async fn deleting_a_workbook_closes_all_its_tables() {
        let gates = Arc::new(TableGates::default());
        let first = gates.enter(BOOK, TABLE).await;
        let second = gates.enter(BOOK, OTHER).await;
        let elsewhere = gates.enter(OTHER, TABLE).await;
        let deletion = tokio::spawn({
            let gates = Arc::clone(&gates);
            async move { gates.close_workbook(BOOK).await }
        });
        for guard in [&first, &second] {
            timeout(SOON, guard.closing()).await.unwrap();
        }
        assert!(!elsewhere.is_closing());
        drop(first);
        assert!(!deletion.is_finished());
        drop(second);
        let closed = timeout(SOON, deletion).await.unwrap().unwrap();
        assert!(gates.try_enter(BOOK, TABLE).is_none());
        drop(closed);
        assert!(gates.try_enter(BOOK, TABLE).is_some());
    }

    #[test]
    fn gates_nobody_is_at_are_dropped() {
        let gates = TableGates::default();
        let held = gates.try_enter(BOOK, TABLE).unwrap();
        for id in 0..3 * PRUNE_AT as u128 {
            let _ = gates.try_enter(Uuid::from_u128(id + 10), TABLE);
        }
        assert!(locked(&gates.workbooks).len() <= PRUNE_AT);
        assert!(gates.try_enter(BOOK, TABLE).is_none());
        drop(held);
    }
}
