//! Workspace architecture-invariant test entry. Each submodule here is a
//! detector that fails the workspace test suite when the corresponding
//! invariant is violated. See AGENTS.md §1 ("ABSOLUTE RULE — SINGLE
//! SOURCE OF TRUTH") for context on why these gates exist.

#[path = "architecture/no_duplicate_state.rs"]
mod no_duplicate_state;

#[path = "architecture/config_save_isolation.rs"]
mod config_save_isolation;

#[path = "architecture/cli_fluent_coverage.rs"]
mod cli_fluent_coverage;

#[path = "architecture/publish_contract.rs"]
mod publish_contract;

// Workflow gates are not armed on this branch.
//
// `release_workflow`, `desktop_release`, `container_release` and
// `ci_runner_labels` all read `.github/workflows/*`, and this branch prunes
// that directory (only `release-cli.yml` survives — see `5dd385bca`). Wired
// up, every one of them panics on a missing file instead of observing an
// invariant, which is noise that also hides real failures elsewhere in this
// binary.
//
// The detector files are kept verbatim so the gates come back untouched the
// moment the workflows do: re-add these four declarations to re-arm them.
//
// #[path = "architecture/release_workflow.rs"]
// mod release_workflow;
//
// #[path = "architecture/desktop_release.rs"]
// mod desktop_release;
//
// #[path = "architecture/container_release.rs"]
// mod container_release;
//
// #[path = "architecture/ci_runner_labels.rs"]
// mod ci_runner_labels;

#[path = "architecture/rpc_proto_boundary.rs"]
mod rpc_proto_boundary;

#[path = "architecture/gateway_feature_edges.rs"]
mod gateway_feature_edges;
