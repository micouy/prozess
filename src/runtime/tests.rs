use super::*;

#[test]
fn fallback_runtime_dir_is_keyed_by_uid_not_user_env() {
    let dir = fallback_runtime_dir();
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("fallback dir should have a utf-8 name");
    let uid = name
        .strip_prefix("pz-")
        .expect("fallback dir should start with pz-");

    assert!(!uid.is_empty(), "{name}");
    assert!(uid.bytes().all(|byte| byte.is_ascii_digit()), "{name}");
}
