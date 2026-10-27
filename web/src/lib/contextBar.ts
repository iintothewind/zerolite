export interface ContextBarState {
  used: number;
  denominator: number;
  percent: number;
  cells: string;
  trimMarkerIndex: number | null;
}

/**
 * Build the provider-route-aware state shared by the context meter UI.
 *
 * The denominator is the **effective context window** — the model's capacity
 * clamped by `[context].request_token_ceiling`. The trim budget (the token
 * count at which a trim engages, and the size it retains to) is NOT a limit:
 * it only marks where trimming starts, so it is drawn as a marker inside the
 * bar.
 */
export function resolveContextBarState(
  contextEffectiveWindow: number | null,
  contextTrimBudgetTokens: number | null,
  contextInputTokens: number | null,
  barWidth = 16,
): ContextBarState | null {
  const denominator = contextEffectiveWindow;
  if (!denominator || denominator <= 0 || barWidth <= 0) return null;

  const used = contextInputTokens ?? 0;
  const percent = Math.min((used / denominator) * 100, 100);
  const filled = Math.round((percent / 100) * barWidth);
  const cells = ('█'.repeat(filled) + '░'.repeat(Math.max(0, barWidth - filled))).split('');
  let trimMarkerIndex: number | null = null;
  if (
    denominator &&
    contextTrimBudgetTokens !== null &&
    contextTrimBudgetTokens > 0 &&
    contextTrimBudgetTokens < denominator
  ) {
    trimMarkerIndex = Math.min(
      Math.round((contextTrimBudgetTokens / denominator) * barWidth),
      barWidth - 1,
    );
    cells[trimMarkerIndex] = '│';
  }

  return { used, denominator, percent, cells: cells.join(''), trimMarkerIndex };
}
