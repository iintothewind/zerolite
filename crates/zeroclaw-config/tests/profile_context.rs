//! The `[runtime_profiles.<alias>.context]` surface: deserialization,
//! round-trip, resolution, and the hard validation errors.

use zeroclaw_config::schema::Config;

const PROFILE_CONTEXT_TOML: &str = r#"
schema_version = 4

[runtime_profiles.coder.context]
request_token_ceiling = 100_000
trim_threshold_percent = 80
reserve_tokens = 16_384
keep_recent_turns = 5

[agents.coder]
enabled = true
runtime_profile = "coder"
model_provider = "anthropic.default"
risk_profile = "default"
"#;

fn profile_config() -> Config {
    toml::from_str(PROFILE_CONTEXT_TOML).expect("the [runtime_profiles.coder.context] table parses")
}

#[test]
fn profile_context_table_deserializes_into_its_own_field() {
    let cfg = profile_config();
    let context = &cfg.runtime_profiles["coder"].context;
    assert_eq!(context.request_token_ceiling, Some(100_000));
    assert_eq!(context.trim_threshold_percent, 80);
    assert_eq!(context.reserve_tokens, Some(16_384));
    assert_eq!(context.keep_recent_turns, Some(5));
}

#[test]
fn profile_context_round_trips_to_the_same_toml_shape() {
    let cfg = profile_config();
    let serialized = toml::to_string(&cfg).expect("the config serializes");
    let reparsed: Config = toml::from_str(&serialized).expect("the serialized config parses back");
    assert_eq!(
        reparsed.runtime_profiles["coder"]
            .context
            .request_token_ceiling,
        Some(100_000)
    );
    assert_eq!(
        reparsed.runtime_profiles["coder"]
            .context
            .trim_threshold_percent,
        80
    );
    assert_eq!(
        reparsed.runtime_profiles["coder"].context.reserve_tokens,
        Some(16_384)
    );
    assert_eq!(
        reparsed.runtime_profiles["coder"].context.keep_recent_turns,
        Some(5)
    );
    // The table must still live under the runtime profile, not somewhere else.
    assert!(serialized.contains("[runtime_profiles.coder.context]"));
}

#[test]
fn absent_context_table_resolves_to_the_documented_defaults() {
    let cfg: Config = toml::from_str(
        r#"
schema_version = 4

[agents.coder]
enabled = true
runtime_profile = "coder"
model_provider = "anthropic.default"
risk_profile = "default"
"#,
    )
    .expect("a config without a context table parses");
    let context = cfg.effective_context_config("coder");
    assert_eq!(context.request_token_ceiling, None);
    assert_eq!(context.trim_threshold_percent, 80);
    assert_eq!(context.reserve_tokens, None);
    assert_eq!(context.keep_recent_turns, None);
}

#[test]
fn out_of_range_trim_threshold_is_a_hard_config_error() {
    for percent in [0, 101] {
        let raw = PROFILE_CONTEXT_TOML.replace(
            "trim_threshold_percent = 80",
            &format!("trim_threshold_percent = {percent}"),
        );
        let cfg: Config = toml::from_str(&raw).expect("the table still parses");
        let error = cfg
            .validate()
            .expect_err("an out-of-range trim_threshold_percent must fail validation");
        let message = error.to_string();
        assert!(
            message.contains("trim_threshold_percent"),
            "the error must name the offending field, got: {message}"
        );
        assert!(
            message.contains("runtime_profiles.coder.context"),
            "the error must carry the offending path, got: {message}"
        );
    }
}

#[test]
fn keep_recent_turns_is_clamped_at_resolution_not_rejected() {
    let mut cfg = profile_config();
    cfg.runtime_profiles
        .get_mut("coder")
        .expect("the profile exists")
        .context
        .keep_recent_turns = Some(999);
    let resolved = cfg
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;
    assert_eq!(resolved.keep_recent_turns(), 10);

    cfg.runtime_profiles
        .get_mut("coder")
        .expect("the profile exists")
        .context
        .keep_recent_turns = Some(0);
    let resolved = cfg
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;
    assert_eq!(resolved.keep_recent_turns(), 1);
}

#[test]
fn the_input_ceiling_clamps_the_effective_context_window() {
    let cfg = profile_config();
    let resolved = cfg
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;
    // The model window is unknown here, so the ceiling is the binding limit.
    assert!(resolved.effective_context_window() <= 100_000);
    assert_eq!(resolved.context.request_token_ceiling, Some(100_000));
}

#[test]
fn the_send_budget_is_the_window_minus_the_reserve() {
    let cfg = profile_config();
    let resolved = cfg
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;
    let window = resolved.effective_context_window();
    assert_eq!(resolved.context_send_budget(), window - 16_384);
    // The water-line never exceeds the send budget: the reserve bounds it.
    assert!(resolved.context_trim_budget() <= resolved.context_send_budget());
}

/// §2 invariant: the water-line equals
/// `min(window × percent / 100, window − reserve)`, and the send budget is
/// `window − reserve`. They coincide only when the reserve is the binding
/// constraint — the two numbers are independently derived.
#[test]
fn the_water_line_is_the_min_of_the_percentage_and_the_reserve() {
    let cfg = profile_config();
    let resolved = cfg
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;
    let window = resolved.effective_context_window();
    let expected = ((window * 80) / 100).min(window - 16_384);
    assert_eq!(resolved.context_trim_budget(), expected);
    assert_eq!(resolved.context_send_budget(), window - 16_384);
    // In this profile the reserve binds, so the two numbers coincide.
    assert_eq!(
        resolved.context_trim_budget(),
        resolved.context_send_budget()
    );

    // With a non-binding reserve they are distinct: engage at 80%, fit under
    // the full window minus a small output reserve.
    let mut cfg = profile_config();
    cfg.runtime_profiles
        .get_mut("coder")
        .expect("the profile exists")
        .context
        .reserve_tokens = Some(1_000);
    let resolved = cfg
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;
    let window = resolved.effective_context_window();
    assert_eq!(resolved.context_trim_budget(), (window * 80) / 100);
    assert_eq!(resolved.context_send_budget(), window - 1_000);
    assert_ne!(
        resolved.context_trim_budget(),
        resolved.context_send_budget(),
        "the water-line and the send budget are two distinct numbers"
    );
}

/// §2 invariant: trimming cannot be disabled — no `[context]` value yields a
/// zero water-line.
#[test]
fn no_context_value_disables_trimming() {
    for percent in [1usize, 50, 80, 100] {
        for ceiling in [None, Some(1usize), Some(1_000)] {
            let mut cfg = profile_config();
            let context = &mut cfg
                .runtime_profiles
                .get_mut("coder")
                .expect("the profile exists")
                .context;
            context.trim_threshold_percent = percent;
            context.request_token_ceiling = ceiling;
            context.reserve_tokens = None;
            let resolved = cfg
                .resolved_agent_config("coder")
                .expect("the agent resolves")
                .resolved;
            assert!(
                resolved.context_trim_budget() > 0,
                "percent={percent} ceiling={ceiling:?} must not disable trimming"
            );
        }
    }
}

/// §2 invariant: `[context]` is the only effective config surface. The
/// upstream knob names are not fields of the runtime profile any more, so
/// writing them cannot change resolution.
#[test]
fn the_removed_upstream_knobs_do_not_change_resolution() {
    let baseline = profile_config();
    let baseline_resolved = baseline
        .resolved_agent_config("coder")
        .expect("the agent resolves")
        .resolved;

    for removed in [
        "max_context_tokens = 1000",
        "context_compact_ratio = 0.5",
        "max_history_messages = 10",
    ] {
        let raw =
            PROFILE_CONTEXT_TOML.replace("[agents.coder]", &format!("{removed}\n\n[agents.coder]"));
        let parsed: Config =
            toml::from_str(&raw).expect("an unknown profile key does not fail deserialization");
        let resolved = parsed
            .resolved_agent_config("coder")
            .expect("the agent resolves")
            .resolved;
        assert_eq!(
            resolved.context_trim_budget(),
            baseline_resolved.context_trim_budget(),
            "`{removed}` must not move the water-line"
        );
        assert_eq!(
            resolved.context_send_budget(),
            baseline_resolved.context_send_budget(),
            "`{removed}` must not move the send budget"
        );
        assert_eq!(
            resolved.keep_recent_turns(),
            baseline_resolved.keep_recent_turns(),
            "`{removed}` must not move the retention count"
        );
    }
}
