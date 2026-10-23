//! Append-only message log with divergence-aware sync.
//!
//! Messages are appended and never rewritten in place. The one sanctioned
//! mutation path is [`AppendOnlyLog::sync`] (reconciling the log with a freshly
//! normalized message list). `sync` preserves the **longest byte-stable
//! prefix**: when a caller prunes, re-renders, or image-strips earlier messages
//! in place, only bytes from the first changed message on are dropped and
//! re-appended — everything before it stays byte-identical so the provider's
//! prefix cache stays warm. An earlier design cleared the whole log on any
//! change, which on local backends forced a ~40k-token re-prefill every turn a
//! single message diverged.

/// A grow-only message sequence whose only rewrites are the two above.
///
/// `M` is the provider-level message payload (generic so this stays decoupled
/// from the runtime's `ChatMessage`). Digests are recomputed from live bytes on
/// every sync, so an in-place mutation of a synced message is always observed —
/// no identity memo that could serve pre-mutation bytes.
#[derive(Debug, Clone)]
pub struct AppendOnlyLog<M> {
    entries: Vec<M>,
    /// Digest of each entry as last synced/replaced — the baseline `sync`
    /// compares the incoming list against.
    digests: Vec<u64>,
}

impl<M> Default for AppendOnlyLog<M> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            digests: Vec::new(),
        }
    }
}

impl<M: serde::Serialize + Clone> AppendOnlyLog<M> {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Direct read access for inspection / serialization.
    #[must_use]
    pub fn entries(&self) -> &[M] {
        &self.entries
    }

    /// Append one message. The hot path: only the new bytes are a cache miss.
    pub fn append(&mut self, message: M) {
        let digest = crate::digest(&message);
        self.entries.push(message);
        self.digests.push(digest);
    }

    /// Append many messages.
    pub fn extend(&mut self, messages: impl IntoIterator<Item = M>) {
        for message in messages {
            self.append(message);
        }
    }

    /// Reconcile the log with `messages` (the freshly normalized, provider-level
    /// list) while preserving the longest byte-stable prefix.
    ///
    /// Cases:
    /// - **Append** (prefix unchanged, longer tail): push the new entries.
    /// - **In-place rewrite / prune** (some earlier message changed bytes):
    ///   truncate back to the first divergence, then re-append from there.
    /// - **Shrink** (the list got shorter, e.g. a trim dropped whole turns):
    ///   every earlier byte may be gone, so the log is rebuilt from scratch.
    ///
    /// Returns the number of entries whose bytes are unchanged from the prior
    /// sync — the cacheable prefix length — so the caller can log a cache-hit
    /// ratio.
    pub fn sync(&mut self, messages: &[M]) -> usize {
        // Shrink: the provider-visible list got shorter, so bytes carried
        // forward are not guaranteed. Rebuild cleanly.
        if messages.len() < self.entries.len() {
            self.clear();
        }

        // Find the first message whose bytes diverge from what we last sent.
        let stable = self.longest_stable_prefix(messages);
        // Drop everything at/after the divergence; it is re-appended below.
        self.truncate(stable);

        // Append the new (or rewritten) tail.
        for message in &messages[stable..] {
            self.append(message.clone());
        }
        stable
    }

    /// Index of the first message in `messages` whose digest differs from the
    /// stored baseline. Equals `min(entries.len(), messages.len())` when the
    /// shared prefix is fully unchanged.
    fn longest_stable_prefix(&self, messages: &[M]) -> usize {
        let bound = self.entries.len().min(messages.len());
        for (i, message) in messages.iter().enumerate().take(bound) {
            if crate::digest(message) != self.digests[i] {
                return i;
            }
        }
        bound
    }

    /// Drop entries at/after `count`. Keeps the first `count` byte-stable.
    fn truncate(&mut self, count: usize) {
        if count >= self.entries.len() {
            return;
        }
        self.entries.truncate(count);
        self.digests.truncate(count);
    }

    /// Remove every entry (used by `sync`'s shrink case and by explicit
    /// resets, e.g. a model switch).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.digests.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, serde::Serialize, PartialEq)]
    struct Msg {
        role: &'static str,
        content: String,
    }

    fn msg(role: &'static str, content: &str) -> Msg {
        Msg {
            role,
            content: content.to_string(),
        }
    }

    #[test]
    fn append_grows_length_and_preserves_order() {
        let mut log = AppendOnlyLog::new();
        log.append(msg("user", "a"));
        log.append(msg("assistant", "b"));
        assert_eq!(log.len(), 2);
        assert_eq!(log.entries()[0].content, "a");
        assert_eq!(log.entries()[1].content, "b");
    }

    #[test]
    fn sync_appends_new_tail_and_reports_stable_prefix() {
        let mut log = AppendOnlyLog::new();
        log.sync(&[msg("user", "a"), msg("assistant", "b")]);
        assert_eq!(log.len(), 2);
        // Next turn: same two + a new one → both prior entries stay stable.
        let stable = log.sync(&[msg("user", "a"), msg("assistant", "b"), msg("user", "c")]);
        assert_eq!(
            stable, 2,
            "the two unchanged messages are the cacheable prefix"
        );
        assert_eq!(log.len(), 3);
        assert_eq!(log.entries()[2].content, "c");
    }

    #[test]
    fn sync_trims_only_back_to_the_divergence_point() {
        // The core cache guarantee: a mid-list in-place rewrite (tool result
        // blanked, image stripped) preserves every earlier byte.
        let mut log = AppendOnlyLog::new();
        log.sync(&[msg("user", "a"), msg("assistant", "b"), msg("user", "c")]);
        // Message index 1 is rewritten in place; indices 0 stays identical.
        let stable = log.sync(&[
            msg("user", "a"),
            msg("assistant", "B REVISED"),
            msg("user", "c"),
        ]);
        assert_eq!(
            stable, 1,
            "only message 0 is byte-stable; divergence at index 1"
        );
        assert_eq!(
            log.len(),
            3,
            "diverged tail is re-appended, length restored"
        );
        assert_eq!(log.entries()[1].content, "B REVISED");
        assert_eq!(
            log.entries()[0].content,
            "a",
            "the earlier byte-stable prefix survives the rewrite"
        );
    }

    #[test]
    fn sync_rebuilds_when_the_list_shrinks() {
        let mut log = AppendOnlyLog::new();
        log.sync(&[msg("user", "a"), msg("assistant", "b"), msg("user", "c")]);
        // A trim dropped the two oldest whole turns and led with its breadcrumb.
        let stable = log.sync(&[msg("system", "breadcrumb"), msg("user", "c")]);
        assert_eq!(stable, 0, "a shorter list carries no guaranteed prefix");
        assert_eq!(log.len(), 2);
        assert_eq!(log.entries()[0].content, "breadcrumb");
    }

    #[test]
    fn empty_sync_on_empty_log_is_noop() {
        let mut log = AppendOnlyLog::<Msg>::new();
        assert_eq!(log.sync(&[]), 0);
        assert!(log.is_empty());
    }
}
