use std::fs;

use tempfile::tempdir;
use zunel_providers::codex::{CodexAuthProvider, FileCodexAuthProvider};

#[tokio::test]
async fn reads_file_backed_codex_auth_from_codex_home() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("auth.json"),
        r#"{
          "auth_mode": "chatgpt",
          "account_id": "acct_fixture",
          "tokens": { "access_token": "access_fixture" },
          "last_refresh": "2026-04-24T00:00:00Z"
        }"#,
    )
    .unwrap();

    let auth = FileCodexAuthProvider::new(dir.path().to_path_buf());
    let token = auth.load().await.unwrap();
    assert_eq!(token.access_token, "access_fixture");
    assert_eq!(token.account_id, "acct_fixture");
}

#[tokio::test]
async fn codex_auth_debug_redacts_access_token() {
    let token = zunel_providers::codex::CodexAuth {
        access_token: "secret-access-token".into(),
        account_id: "acct_fixture".into(),
    };

    let rendered = format!("{token:?}");
    assert!(!rendered.contains("secret-access-token"), "{rendered}");
    assert!(rendered.contains("acct_fixture"), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
}

#[tokio::test]
async fn accepts_nested_account_id_shapes_seen_in_codex_auth_files() {
    let cases = [
        (r#""chatgpt_account_id": "acct_top""#, "acct_top"),
        (r#""account": { "id": "acct_account" }"#, "acct_account"),
        (
            r#""profile": { "account_id": "acct_profile" }"#,
            "acct_profile",
        ),
        (
            r#""tokens": { "access_token": "access_fixture", "account_id": "acct_tokens" }"#,
            "acct_tokens",
        ),
    ];

    for (account_fragment, expected_account) in cases {
        let dir = tempdir().unwrap();
        let tokens_fragment = if account_fragment.contains(r#""tokens""#) {
            account_fragment.to_string()
        } else {
            format!(r#""tokens": {{ "access_token": "access_fixture" }}, {account_fragment}"#)
        };
        fs::write(
            dir.path().join("auth.json"),
            format!(
                r#"{{
                  "auth_mode": "chatgpt",
                  {tokens_fragment}
                }}"#
            ),
        )
        .unwrap();

        let auth = FileCodexAuthProvider::new(dir.path().to_path_buf());
        let token = auth.load().await.unwrap();
        assert_eq!(token.access_token, "access_fixture");
        assert_eq!(token.account_id, expected_account);
    }
}

#[tokio::test]
async fn missing_auth_file_returns_login_hint() {
    let dir = tempdir().unwrap();
    let auth = FileCodexAuthProvider::new(dir.path().to_path_buf());
    let err = auth.load().await.unwrap_err().to_string();
    assert!(err.contains("Codex OAuth credentials unavailable"), "{err}");
    assert!(err.contains("codex login"), "{err}");
}

#[tokio::test]
async fn missing_access_token_returns_login_hint() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("auth.json"),
        r#"{
          "auth_mode": "chatgpt",
          "account_id": "acct_fixture",
          "tokens": {}
        }"#,
    )
    .unwrap();

    let auth = FileCodexAuthProvider::new(dir.path().to_path_buf());
    let err = auth.load().await.unwrap_err().to_string();
    assert!(err.contains("access token"), "{err}");
    assert!(err.contains("codex login"), "{err}");
}

#[tokio::test]
async fn missing_account_id_returns_login_hint() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("auth.json"),
        r#"{
          "auth_mode": "chatgpt",
          "tokens": { "access_token": "access_fixture" }
        }"#,
    )
    .unwrap();

    let auth = FileCodexAuthProvider::new(dir.path().to_path_buf());
    let err = auth.load().await.unwrap_err().to_string();
    assert!(err.contains("account id"), "{err}");
    assert!(err.contains("codex login"), "{err}");
}
