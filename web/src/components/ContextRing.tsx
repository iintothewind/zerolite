import { useEffect, useRef, useState } from 'react';
import { t } from '@/lib/i18n';

interface ContextRingProps {
  /** Prompt size of the most recent LLM call, or null before the first `done`. */
  used: number | null;
  /**
   * The effective context window — the model's capacity clamped by
   * `[context].request_token_ceiling` — or null until the gateway reports it.
   * This is the meter denominator; the water-line (where trimming engages) is
   * a trigger, not this limit.
   */
  max: number | null;
  /**
   * The water-line: the token count at which proactive trimming engages. It
   * is a trigger, not a limit — shown so the two numbers are never confused.
   */
  waterline?: number | null;
}

const SIZE = 22;
const STROKE = 3;
const RADIUS = (SIZE - STROKE) / 2;
const CIRCUMFERENCE = 2 * Math.PI * RADIUS;

function formatExactTokens(n: number): string {
  return n.toLocaleString();
}

/**
 * Circular context-occupancy meter for the composer, replacing the footer's
 * linear `ContextBar`. Clicking it opens a panel with the exact figures.
 *
 * Degrades to an empty ring when the gateway has not reported a window yet
 * (a freshly loaded conversation, before any `done` frame) rather than
 * inventing a fill — the ring is a meter, not a progress indicator.
 */
export default function ContextRing({ used, max, waterline }: ContextRingProps) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  // Dismiss on outside click / Escape. A meter that only closes on a second
  // click on itself reads as stuck.
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (ev: MouseEvent) => {
      if (!rootRef.current?.contains(ev.target as Node)) setOpen(false);
    };
    const onKeyDown = (ev: KeyboardEvent) => {
      if (ev.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('mousedown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open]);

  const hasWindow = typeof max === 'number' && max > 0;
  // The water-line is only worth showing when it is strictly below the limit
  // (it always is, unless the percentage is 100).
  const hasWaterline = typeof waterline === 'number' && waterline > 0 && waterline < (max ?? 0);
  const usedTokens = typeof used === 'number' && used > 0 ? used : 0;
  const pct = hasWindow ? Math.min((usedTokens / (max as number)) * 100, 100) : 0;
  // The templates carry their own `%`, so they take the bare number; only the
  // title wants the sign, and it is not a template.
  const pctNumber = Math.round(pct);
  const pctLabel = `${pctNumber}%`;

  return (
    <div ref={rootRef} className="relative flex-shrink-0">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-label={t('agent.context.ring_label')}
        aria-expanded={open}
        title={`${t('agent.context.ring_label')} ${hasWindow ? pctLabel : ''}`.trim()}
        className="flex h-10 w-10 items-center justify-center rounded-full transition-colors hover:bg-pc-elevated focus:outline-none focus-visible:ring-2 focus-visible:ring-pc-accent/40"
      >
        <svg width={SIZE} height={SIZE} viewBox={`0 0 ${SIZE} ${SIZE}`} aria-hidden>
          <circle
            cx={SIZE / 2}
            cy={SIZE / 2}
            r={RADIUS}
            fill="none"
            stroke="var(--pc-border)"
            strokeWidth={STROKE}
          />
          {hasWindow && pct > 0 && (
            <circle
              cx={SIZE / 2}
              cy={SIZE / 2}
              r={RADIUS}
              fill="none"
              stroke="var(--pc-accent)"
              strokeWidth={STROKE}
              strokeLinecap="round"
              strokeDasharray={CIRCUMFERENCE}
              strokeDashoffset={CIRCUMFERENCE * (1 - pct / 100)}
              // Start the fill at 12 o'clock instead of 3 o'clock.
              transform={`rotate(-90 ${SIZE / 2} ${SIZE / 2})`}
            />
          )}
        </svg>
      </button>

      {open && (
        <div
          className="absolute bottom-full right-0 z-50 mb-2 min-w-[180px] rounded-xl border py-2 shadow-lg"
          style={{ background: 'var(--pc-bg-elevated)', borderColor: 'var(--pc-border)' }}
        >
          <div
            className="px-3 py-1 text-[10px] uppercase tracking-wide"
            style={{ color: 'var(--pc-text-faint)' }}
          >
            {t('agent.context.ring_label')}
          </div>
          <div
            className="px-3 py-1 text-xs font-mono tabular-nums"
            style={{ color: 'var(--pc-text-primary)' }}
          >
            {t('agent.context.ring_detail_used').replace('{count}', formatExactTokens(usedTokens))}
          </div>
          <div
            className="px-3 py-1 text-xs font-mono tabular-nums"
            style={{ color: 'var(--pc-text-secondary)' }}
          >
            {t('agent.context.ring_detail_max').replace(
              '{count}',
              hasWindow ? formatExactTokens(max as number) : '—',
            )}
          </div>
          {hasWaterline && (
            <div
              className="px-3 py-1 text-xs font-mono tabular-nums"
              style={{ color: 'var(--pc-text-secondary)' }}
            >
              {t('agent.context.ring_detail_waterline').replace(
                '{count}',
                formatExactTokens(waterline as number),
              )}
            </div>
          )}
          {hasWindow && (
            <div
              className="px-3 py-1 text-xs font-mono tabular-nums"
              style={{ color: 'var(--pc-text-secondary)' }}
            >
              {t('agent.context.ring_detail_percent').replace('{percent}', String(pctNumber))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
