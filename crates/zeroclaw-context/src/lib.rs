//! # zeroclaw-context
//!
//! Context-cache economics core: **stable-prefix freezing** + an **append-only
//! message log**. Both exist to keep a provider's prefix cache warm across
//! turns, since context management *is* cache management — every byte that
//! changes in the replayed prefix forces the provider to re-prefill it.
//!
//! - [`StablePrefix`] freezes the system prompt + tool set into a
//!   byte-stable, fingerprinted snapshot. It rebuilds only when the live
//!   state's fingerprint changes or [`StablePrefix::invalidate`] is called
//!   (MCP tool-set change, model switch).
//! - [`AppendOnlyLog`] grows a message sequence and never rewrites it except
//!   through [`AppendOnlyLog::sync`], which preserves the longest byte-stable
//!   prefix. Shrinking (a trim that dropped whole turns) and in-place pruning
//!   both go through the same call, which trims only back to the divergence
//!   point — the earlier bytes stay cacheable.
//!
//! [`sync`]: AppendOnlyLog::sync
//!
//! Portable and dependency-light: generic over any `serde::Serialize` prefix
//! and message payload, with no coupling to the rest of ZeroClaw so it can be
//! reasoned about — and unit-tested — in isolation. The runtime adapter that
//! wraps the real `ChatMessage` / tool-spec types lives in `zeroclaw-runtime`;
//! this crate is the mechanism, not the policy.

pub mod log;
pub mod prefix;

pub use log::AppendOnlyLog;
pub use prefix::StablePrefix;

/// 64-bit FNV-1a over `serde_json` bytes of a value — the shared digest both
/// the prefix fingerprint and the per-message digest use. Deterministic for a
/// given logical value; recomputed from live bytes on every call so a mutated
/// value never reads stale (the cache-coherence hazard the JS original papers
/// over with an identity memo).
pub(crate) fn digest<T: serde::Serialize>(value: &T) -> u64 {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for b in &bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
