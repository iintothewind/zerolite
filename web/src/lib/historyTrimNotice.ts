import type { WsMessage } from '@/types/api';

type Translate = (key: string) => string;

export function formatHistoryTrimmedNotice(msg: WsMessage, translate: Translate): string {
  const kept = msg.kept_turns ?? 0;
  const droppedTurns = msg.dropped_turns;

  if (droppedTurns === undefined) {
    return translate('agent.history_trimmed')
      .replace('{dropped}', String(msg.dropped_messages ?? 0))
      .replace('{kept}', String(kept));
  }

  const droppedUnit = translate(
    droppedTurns === 1
      ? 'agent.history_trimmed_turn_singular'
      : 'agent.history_trimmed_turn_plural',
  );
  return translate('agent.history_trimmed_turns')
    .replace('{dropped}', String(droppedTurns))
    .replace('{dropped_unit}', droppedUnit)
    .replace('{kept}', String(kept));
}
