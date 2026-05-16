use zunel_core::{CommandContext, CommandOutcome, CommandRouter};

#[test]
fn exact_match_dispatches() {
    let mut router = CommandRouter::new();
    router.register_exact("/help", |_ctx| {
        Box::pin(async move { Ok(CommandOutcome::Reply("Available commands: /help".into())) })
    });

    let ctx = CommandContext {
        session_key: "cli:direct".into(),
        raw: "/help".into(),
        args: String::new(),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = rt.block_on(router.dispatch(&ctx));
    match outcome {
        Ok(Some(CommandOutcome::Reply(s))) => assert!(s.contains("/help")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn unknown_command_returns_none() {
    let router = CommandRouter::new();
    let ctx = CommandContext {
        session_key: "cli:direct".into(),
        raw: "/does-not-exist".into(),
        args: String::new(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(rt.block_on(router.dispatch(&ctx)).unwrap().is_none());
}

#[test]
fn prefix_match_dispatches_with_args() {
    let mut router = CommandRouter::new();
    router.register_prefix("/echo ", |ctx| {
        Box::pin(async move { Ok(CommandOutcome::Reply(ctx.args.clone())) })
    });
    let ctx = CommandContext {
        session_key: "cli:direct".into(),
        raw: "/echo hello world".into(),
        args: String::new(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    match rt.block_on(router.dispatch(&ctx)).unwrap() {
        Some(CommandOutcome::Reply(s)) => assert_eq!(s, "hello world"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn not_a_command_returns_none() {
    let mut router = CommandRouter::new();
    router.register_exact("/help", |_| {
        Box::pin(async { Ok(CommandOutcome::Reply("help".into())) })
    });
    let ctx = CommandContext {
        session_key: "cli:direct".into(),
        raw: "regular message".into(),
        args: String::new(),
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(rt.block_on(router.dispatch(&ctx)).unwrap().is_none());
}

#[test]
fn builtin_help_lists_known_commands() {
    use zunel_core::command::builtins::help_text;
    let text = help_text();
    for cmd in ["/help", "/clear", "/status", "/restart", "/exit", "/quit"] {
        assert!(text.contains(cmd), "missing {cmd} in help:\n{text}");
    }
}

/// `/exit` and `/quit` must round-trip through `register_defaults`
/// to `CommandOutcome::Exit` so the REPL break-loop wires up. Pin
/// both names: users guess one or the other.
#[test]
fn builtin_register_defaults_wires_exit_and_quit() {
    use zunel_core::command::builtins::register_defaults;
    let mut router = CommandRouter::new();
    register_defaults(&mut router);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for cmd in ["/exit", "/quit"] {
        let ctx = CommandContext {
            session_key: "cli:direct".into(),
            raw: cmd.into(),
            args: String::new(),
        };
        match rt.block_on(router.dispatch(&ctx)).unwrap() {
            Some(CommandOutcome::Exit) => {}
            other => panic!("expected Exit for {cmd}, got: {other:?}"),
        }
    }
}

/// `/reload` with no argument should request a full reload (target =
/// `None`); `/reload <server>` should target one server. Pin both
/// shapes — the CLI REPL uses this enum to decide whether to call
/// `AgentLoop::reload_mcp(None, ..)` or `Some(name)`.
#[test]
fn builtin_register_defaults_wires_reload() {
    use zunel_core::command::builtins::register_defaults;
    let mut router = CommandRouter::new();
    register_defaults(&mut router);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let ctx_all = CommandContext {
        session_key: "cli:direct".into(),
        raw: "/reload".into(),
        args: String::new(),
    };
    match rt.block_on(router.dispatch(&ctx_all)).unwrap() {
        Some(CommandOutcome::ReloadMcp { target }) => assert!(target.is_none()),
        other => panic!("expected ReloadMcp(None) for /reload, got: {other:?}"),
    }

    let ctx_one = CommandContext {
        session_key: "cli:direct".into(),
        raw: "/reload redlab".into(),
        args: String::new(),
    };
    match rt.block_on(router.dispatch(&ctx_one)).unwrap() {
        Some(CommandOutcome::ReloadMcp { target }) => {
            assert_eq!(target.as_deref(), Some("redlab"));
        }
        other => panic!("expected ReloadMcp(Some(redlab)) for /reload redlab, got: {other:?}"),
    }
}

/// Help text must list `/reload` so users discover it via `/help`.
#[test]
fn builtin_help_lists_reload() {
    use zunel_core::command::builtins::help_text;
    assert!(
        help_text().contains("/reload"),
        "help text missing `/reload`:\n{}",
        help_text()
    );
}
