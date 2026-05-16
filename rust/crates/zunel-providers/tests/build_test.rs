use zunel_config::{
    AgentDefaults, AgentsConfig, CodexProvider, Config, CustomProvider, ProvidersConfig,
};
use zunel_providers::build_provider;

fn config_with_custom() -> Config {
    Config {
        providers: ProvidersConfig {
            custom: Some(CustomProvider {
                api_key: "sk".into(),
                api_base: "https://x.test".into(),
                extra_headers: None,
            }),
            codex: None,
            bedrock: None,
        },
        agents: AgentsConfig {
            defaults: AgentDefaults {
                provider: Some("custom".into()),
                model: "gpt-x".into(),
                ..Default::default()
            },
        },
        channels: Default::default(),
        gateway: Default::default(),
        tools: Default::default(),
        cli: Default::default(),
        ..Default::default()
    }
}

#[tokio::test]
async fn builds_custom_provider_from_config() {
    let cfg = config_with_custom();
    let _provider = build_provider(&cfg).await.expect("builds");
}

#[tokio::test]
async fn builds_codex_provider_without_requiring_api_key() {
    let mut cfg = config_with_custom();
    cfg.agents.defaults.provider = Some("codex".into());
    cfg.providers.custom = None;
    cfg.providers.codex = Some(CodexProvider {
        api_base: Some("https://chatgpt.example/backend-api/codex/responses".into()),
    });
    let _provider = build_provider(&cfg).await.expect("builds codex");
}

#[tokio::test]
async fn errors_when_no_provider_configured() {
    let cfg = Config::default();
    let err = build_provider(&cfg).await.err().expect("expected Err");
    assert!(matches!(err, zunel_providers::Error::Config(_)));
}
