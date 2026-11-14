/**
 * History-trim notice rendering, extracted from AgentContext so the web
 * rendering boundary is unit-testable without a React/browser harness.
 *
 * The notice is the counts sentence (`formatHistoryTrimmedNotice`), wrapped
 * by a size message when `tokens_before`/`tokens_after` are present, with the
 * budget appended when one was in effect. Counts use the compact
 * `formatTokens` form; a `~` marks a count whose source is a local estimate.
 * The trim reason is not rendered.
 */

import { formatHistoryTrimmedNotice } from '../lib/historyTrimNotice.ts';
import { formatTokens } from '../lib/sessionStats.logic.ts';

export interface HistoryTrimmedNoticeMessage {
  dropped_turns?: number;
  reason?: string;
  dropped_messages?: number;
  kept_turns?: number;
  token_budget?: number;
  tokens_before?: number;
  tokens_after?: number;
  tokens_before_source?: string;
  tokens_after_source?: string;
  unsatisfiable_floor?: boolean;
}

export function buildHistoryTrimmedNotice(
  msg: HistoryTrimmedNoticeMessage,
  t: (key: string) => string,
): string {
  const tokens = (count: number, source?: string) =>
    `${source === 'estimate' ? '~' : ''}${formatTokens(count)}`;

  const { tokens_before: before, tokens_after: after, token_budget: budget } = msg;
  const counts = formatHistoryTrimmedNotice({ type: 'history_trimmed', ...msg }, t);
  if (before == null || after == null) return counts;
  // The unsatisfiable newest-turn/schema floor is flagged explicitly by the
  // runtime: the retained request cannot fit the configured budget even though
  // history MAY have been trimmed on the way to that floor. The flag — not
  // `dropped_messages === 0` — is the authoritative signal, so a floor reached
  // after real drops still renders truthfully.
  if (msg.unsatisfiable_floor === true && budget != null) {
    return t('agent.history_trimmed_floor')
      .replace('{after}', tokens(after, msg.tokens_after_source))
      .replace('{budget}', formatTokens(budget));
  }
  const sized = t(budget != null ? 'agent.history_trimmed_size_budget' : 'agent.history_trimmed_size')
    .replace('{notice}', () => counts)
    .replace('{before}', tokens(before, msg.tokens_before_source))
    .replace('{after}', tokens(after, msg.tokens_after_source));
  return budget != null ? sized.replace('{budget}', formatTokens(budget)) : sized;
}
