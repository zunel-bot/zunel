#[test]
fn ensure_dir_creates_missing_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("a/b/c");
    zunel_util::ensure_dir(&target).unwrap();
    assert!(target.is_dir());
}

#[test]
fn ensure_dir_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("same");
    zunel_util::ensure_dir(&target).unwrap();
    zunel_util::ensure_dir(&target).unwrap();
    assert!(target.is_dir());
}
