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
