use super::*;

#[test]
fn current_process_has_a_token() {
    let token = current_token(std::process::id());
    assert!(token.is_some());
    // Sanity: a plausible Unix timestamp, not ticks since boot.
    assert!(token.unwrap() > 1_000_000_000, "{token:?}");
}

#[test]
fn is_alive_matches_only_the_same_incarnation() {
    let pid = std::process::id();
    let token = current_token(pid);

    assert!(is_alive(pid, token));
    assert!(is_alive(pid, None));
    // Same pid, different start time: a recycled pid.
    assert!(!is_alive(pid, Some(token.unwrap() + 1)));
}

#[test]
fn dead_process_is_not_alive() {
    let child = std::process::Command::new("/usr/bin/true")
        .spawn()
        .and_then(|mut child| child.wait().map(|_| child.id()));
    let pid = child.expect("failed to run /usr/bin/true");

    // Even if the pid got recycled instantly, no process started in
    // 1970, so the token comparison must fail.
    assert!(!is_alive(pid, Some(1)));
}
