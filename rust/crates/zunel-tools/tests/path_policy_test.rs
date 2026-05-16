use tempfile::tempdir;

use zunel_tools::path_policy::PathPolicy;

#[test]
fn absolute_under_workspace_is_allowed() {
    let ws = tempdir().unwrap();
    let policy = PathPolicy::restricted(ws.path());
    let target = ws.path().join("a.txt");
    assert!(policy.check(&target).is_ok());
}

#[test]
fn absolute_outside_workspace_is_denied_when_restricted() {
    let ws = tempdir().unwrap();
    let other = tempdir().unwrap();
    let policy = PathPolicy::restricted(ws.path());
    let err = policy.check(&other.path().join("x.txt")).unwrap_err();
    assert!(err.to_string().contains("outside workspace"), "{err}");
}

#[test]
fn unrestricted_allows_any_path() {
    let other = tempdir().unwrap();
    let policy = PathPolicy::unrestricted();
    assert!(policy.check(&other.path().join("x.txt")).is_ok());
}

#[test]
fn media_dir_escape_hatch_allows_subpaths() {
    let ws = tempdir().unwrap();
    let media = tempdir().unwrap();
    let policy = PathPolicy::restricted(ws.path()).with_media_dir(media.path());
    assert!(policy.check(&media.path().join("file.png")).is_ok());
    let err = policy
        .check(&media.path().parent().unwrap().join("elsewhere"))
        .unwrap_err();
    assert!(err.to_string().contains("outside workspace"), "{err}");
}

#[cfg(unix)]
#[test]
fn symlink_inside_workspace_pointing_outside_is_denied() {
    // Attack: a model writes a symlink inside the workspace that targets
    // /etc/passwd (or anywhere outside), then asks the read_file / write_file
    // tools to follow it. The syntactic check sees a path under the workspace
    // and lets it through; only canonicalisation reveals the escape.
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, b"top secret").unwrap();
    let link = ws.path().join("backdoor");
    std::os::unix::fs::symlink(&outside_file, &link).unwrap();

    let policy = PathPolicy::restricted(ws.path());
    let err = policy
        .check(&link)
        .expect_err("symlink pointing outside workspace must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("outside workspace") || msg.contains("symlink"),
        "expected outside-workspace error, got {msg}"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_workspace_subdir_pointing_outside_is_denied() {
    // Variant: the symlink is a directory; a path "under" it looks fine
    // syntactically but resolves outside the workspace.
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let link_dir = ws.path().join("exfil");
    std::os::unix::fs::symlink(outside.path(), &link_dir).unwrap();

    let policy = PathPolicy::restricted(ws.path());
    let err = policy
        .check(&link_dir.join("loot.txt"))
        .expect_err("path under a directory symlink pointing outside must be rejected");
    assert!(err.to_string().contains("outside workspace"), "{err}");
}

#[cfg(unix)]
#[test]
fn symlink_inside_workspace_pointing_inside_is_allowed() {
    // Legitimate use: a symlink that stays inside the workspace must still
    // pass — we are blocking *escapes*, not all symlinks.
    let ws = tempdir().unwrap();
    let real = ws.path().join("real");
    std::fs::write(&real, b"hi").unwrap();
    let link = ws.path().join("alias");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let policy = PathPolicy::restricted(ws.path());
    policy
        .check(&link)
        .expect("in-workspace symlink must still be accepted");
}

#[test]
fn non_existing_path_under_workspace_is_allowed() {
    // Writes target files that don't yet exist; the policy must not reject
    // them just because canonicalize() fails on a missing leaf.
    let ws = tempdir().unwrap();
    let target = ws.path().join("subdir/new_file.txt");
    let policy = PathPolicy::restricted(ws.path());
    policy
        .check(&target)
        .expect("missing file should be allowed");
}
