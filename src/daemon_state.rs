use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::Mutex;

#[derive(Debug, Clone, Default)]
pub struct DaemonState {
    inner: Arc<Mutex<DaemonStateInner>>,
}

#[derive(Debug, Default)]
struct DaemonStateInner {
    processes: BTreeMap<i64, RuntimeProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProcess {
    pub id: i64,
    pub pid: u32,
    pub pgid: u32,
}

impl DaemonState {
    pub async fn insert_process(&self, process: RuntimeProcess) {
        self.inner
            .lock()
            .await
            .processes
            .insert(process.id, process);
    }

    pub async fn remove_process(&self, id: i64) {
        self.inner.lock().await.processes.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracks_and_removes_runtime_processes() {
        let state = DaemonState::default();
        state
            .insert_process(RuntimeProcess {
                id: 1,
                pid: 123,
                pgid: 123,
            })
            .await;
        assert_eq!(state.inner.lock().await.processes.len(), 1);

        state.remove_process(1).await;
        assert!(state.inner.lock().await.processes.is_empty());
    }
}
