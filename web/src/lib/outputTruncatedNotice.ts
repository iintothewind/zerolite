import type { WsMessage } from '@/types/api';

type Translate = (key: string) => string;

/**
 * Text of the display-only `output_truncated` notice: the provider stopped
 * the reply at its output limit. Distinct from the history-trim notice, which
 * reports input dropped from the context.
 */
export function formatOutputTruncatedNotice(msg: WsMessage, translate: Translate): string {
  const reason = msg.reason || 'length';
  const model = msg.model || '?';
  const key = typeof msg.output_tokens === 'number'
    ? 'agent.output_truncated'
    : 'agent.output_truncated_no_tokens';
  return translate(key)
    .replace('{reason}', reason)
    .replace('{model}', model)
    .replace('{tokens}', String(msg.output_tokens ?? ''));
}
