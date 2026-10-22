//! The `gateway.master_pair_code` recovery credential, end to end.
//!
//! The unit tests in `pairing.rs` exercise the guard's API in isolation: they
//! build one by hand and call `try_master` directly. That misses the part
//! that actually broke — whether the *configured* value reaches the guard the
//! gateway serves pairing requests from. A production path that builds its
//! guard without the master code disables the credential silently: the
//! pairing page rejects the code with "Invalid or expired pairing code" and
//! nothing says the value was configured at all.
//!
//! These tests therefore go through the configuration: a real `config.toml`,
//! parsed the way the gateway parses it, into the guard the gateway serves.

use zeroclaw_config::pairing::PairingGuard;
use zeroclaw_config::schema::Config;

const MASTER_PAIR_CODE_TOML: &str = r#"
schema_version = 4

[gateway]
require_pairing = true
master_pair_code = "a-long-random-recovery-string"
"#;

fn gateway_config(toml_source: &str) -> Config {
    toml::from_str(toml_source).expect("the config parses")
}

#[test]
fn a_configured_master_pair_code_reaches_the_served_guard() {
    let cfg = gateway_config(MASTER_PAIR_CODE_TOML);
    assert_eq!(
        cfg.gateway.master_pair_code.as_deref(),
        Some("a-long-random-recovery-string"),
        "the configured recovery credential must survive deserialization"
    );

    // The guard the gateway pairs against, built the way both production
    // paths build it.
    let guard = PairingGuard::from_gateway_config(&cfg.gateway);
    assert!(
        guard.has_master_code(),
        "the configured master code must reach the guard"
    );

    // It must actually work: the master code pairs without consuming the
    // one-time startup code, and it stays reusable.
    let token = guard
        .try_master("a-long-random-recovery-string")
        .expect("the configured master code must pair");
    assert!(token.starts_with("zc_"));
    assert!(guard.is_authenticated(&token));
    assert!(
        guard.try_master("a-long-random-recovery-string").is_some(),
        "the master code is reusable"
    );
}

#[test]
fn a_wrong_code_does_not_pair_through_the_master_path() {
    let cfg = gateway_config(MASTER_PAIR_CODE_TOML);
    let guard = PairingGuard::from_gateway_config(&cfg.gateway);
    assert!(guard.try_master("not-the-master").is_none());
}

#[test]
fn an_unset_master_pair_code_leaves_the_guard_without_one() {
    let cfg = gateway_config(
        r#"
schema_version = 4

[gateway]
require_pairing = true
"#,
    );
    let guard = PairingGuard::from_gateway_config(&cfg.gateway);
    assert!(!guard.has_master_code());
    assert!(guard.try_master("anything").is_none());
}

#[test]
fn a_master_pair_code_is_inert_while_pairing_is_disabled() {
    // Pairing off means the gateway already accepts every request, so there is
    // no gate for a recovery credential to bypass.
    let cfg = gateway_config(
        r#"
schema_version = 4

[gateway]
require_pairing = false
master_pair_code = "a-long-random-recovery-string"
"#,
    );
    let guard = PairingGuard::from_gateway_config(&cfg.gateway);
    assert!(guard.has_master_code(), "the code is still configured");
    assert!(
        guard.try_master("a-long-random-recovery-string").is_none(),
        "the master code must be inert while pairing is disabled"
    );
}
