use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard, PoisonError},
};

use super::JobEntry;

/// The claimed jobs the worker of this process holds, from the claim to the write of their
/// outcome (waiting in the channel or running). The heartbeat renews only their leases: a
/// claimed job missing here (an outcome not written, a claim whose answer was lost) is released
/// when its lease ends.
#[derive(Debug, Default)]
pub struct Held(Mutex<HashSet<JobEntry>>);

impl Held {
    pub fn add(&self, entries: impl IntoIterator<Item = JobEntry>) {
        self.lock().extend(entries);
    }

    pub fn remove(&self, entries: impl IntoIterator<Item = JobEntry>) {
        let mut held = self.lock();
        for entry in entries {
            held.remove(&entry);
        }
    }

    /// The ids and the attempts held now, as two columns for `unnest`.
    #[must_use]
    pub fn columns(&self) -> (Vec<i64>, Vec<i32>) {
        self.lock().iter().map(|e| (e.id, e.attempt)).unzip()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<JobEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[test]
fn an_entry_is_held_until_removed() {
    let held = Held::default();
    let (a, b) = (
        JobEntry { id: 1, attempt: 0 },
        JobEntry { id: 2, attempt: 3 },
    );
    held.add([a, b]);
    held.remove([a, JobEntry { id: 2, attempt: 2 }]);
    assert_eq!(held.columns(), (vec![2], vec![3]));
}
