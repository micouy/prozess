use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use tokio::{sync::watch, task::JoinHandle};

#[derive(Debug, Clone, Default)]
pub struct DaemonState {
    inner: Arc<Mutex<DaemonStateInner>>,
}

#[derive(Debug, Default)]
struct DaemonStateInner {
    processes: BTreeMap<i64, RuntimeProcess>,
}

#[derive(Debug)]
struct RuntimeProcess {
    metadata: RuntimeProcessMetadata,
    lifecycle: watch::Sender<ProcessLifecycle>,
    timeout: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeProcessMetadata {
    pub id: i64,
    pub pid: u32,
    pub pgid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessLifecycle {
    Running,
    Finished { exit_code: Option<i32> },
}

impl DaemonState {
    pub fn insert_process(&self, metadata: RuntimeProcessMetadata) {
        let (lifecycle, _) = watch::channel(ProcessLifecycle::Running);

        self.inner
            .lock()
            .expect("daemon state poisoned")
            .processes
            .insert(
                metadata.id,
                RuntimeProcess {
                    metadata,
                    lifecycle,
                    timeout: None,
                },
            );
    }

    pub fn finish_process(&self, id: i64, exit_code: Option<i32>) {
        if let Some(mut process) = self
            .inner
            .lock()
            .expect("daemon state poisoned")
            .processes
            .remove(&id)
        {
            if let Some(timeout) = process.timeout.take() {
                timeout.abort();
            }

            let _ = process
                .lifecycle
                .send(ProcessLifecycle::Finished { exit_code });
        }
    }

    pub fn set_timeout(&self, id: i64, timeout: Option<JoinHandle<()>>) {
        if let Some(process) = self
            .inner
            .lock()
            .expect("daemon state poisoned")
            .processes
            .get_mut(&id)
        {
            if let Some(existing) = process.timeout.take() {
                existing.abort();
            }

            process.timeout = timeout;
        }
    }

    pub fn process(&self, id: i64) -> Option<RuntimeProcessMetadata> {
        self.inner
            .lock()
            .expect("daemon state poisoned")
            .processes
            .get(&id)
            .map(|process| process.metadata.clone())
    }

    #[allow(dead_code)]
    pub fn subscribe(&self, id: i64) -> Option<watch::Receiver<ProcessLifecycle>> {
        self.inner
            .lock()
            .expect("daemon state poisoned")
            .processes
            .get(&id)
            .map(|process| process.lifecycle.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tracks_and_notifies_runtime_process_completion() {
        let state = DaemonState::default();
        state.insert_process(RuntimeProcessMetadata {
            id: 1,
            pid: 123,
            pgid: 123,
        });
        assert_eq!(state.inner.lock().unwrap().processes.len(), 1);

        let mut lifecycle = state.subscribe(1).expect("process should be registered");
        state.finish_process(1, Some(0));

        lifecycle.changed().await.unwrap();
        assert_eq!(
            *lifecycle.borrow(),
            ProcessLifecycle::Finished { exit_code: Some(0) }
        );
        assert!(state.inner.lock().unwrap().processes.is_empty());
    }

    #[tokio::test]
    async fn replacing_timeout_aborts_previous_handle() {
        let state = DaemonState::default();
        state.insert_process(RuntimeProcessMetadata {
            id: 1,
            pid: 123,
            pgid: 123,
        });

        let first = tokio::spawn(async { std::future::pending::<()>().await });
        let first_abort = first.abort_handle();
        state.set_timeout(1, Some(first));

        let second = tokio::spawn(async { std::future::pending::<()>().await });
        let second_abort = second.abort_handle();
        state.set_timeout(1, Some(second));
        tokio::task::yield_now().await;

        assert!(first_abort.is_finished());
        assert!(!second_abort.is_finished());
        state.set_timeout(1, None);
        tokio::task::yield_now().await;
        assert!(second_abort.is_finished());
    }
}
