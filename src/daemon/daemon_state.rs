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
mod tests;
