import { Fragment } from 'react';
import { t } from '@/lib/i18n';
import {
  cacheHitRatio,
  formatRate,
  type LiveStats,
} from '@/lib/sessionStats.logic';

interface SessionStatsRowProps {
  stats: LiveStats;
  compact?: boolean;
}

interface Segment {
  text: string;
  /** Tooltip flag for a reading the provider never billed (`est`). */
  title?: string;
  /** Terminal readings stay on screen, dimmed, until the next turn replaces them. */
  faint?: boolean;
}

/** Renders `N turns · M steps · cache hit …% · X tok/s`. The rate segment is
 *  the live meter reading (`TokenRateMeter`); it is omitted while the meter
 *  has no evidence, never a `0 tok/s` placeholder. Canonical rule:
 *  `docs/design/webui.md` → "Composer stats semantics". */
export default function SessionStatsRow({ stats, compact = false }: SessionStatsRowProps) {
  const segments: Segment[] = [
    {
      text: `${t('agent.stats.turns').replace('{count}', String(stats.turns))} ${t('agent.stats.steps').replace('{count}', String(stats.steps))}`,
    },
  ];
  segments.push({
    text: t('agent.stats.cache_hit').replace(
      '{percent}',
      String(Math.round(cacheHitRatio(stats) * 100)),
    ),
  });
  if (stats.rate !== null) {
    segments.push({
      text: t('agent.stats.rate').replace('{rate}', formatRate(stats.rate)),
      title: stats.rateBilled ? undefined : t('agent.stats.rate_est'),
      faint: stats.rateMuted,
    });
  }

  return (
    <div
      className={compact
        ? 'min-w-0 overflow-hidden text-[11px] font-mono leading-none'
        : 'flex min-w-0 flex-1 items-center text-[11px] font-mono'}
      style={{ color: 'var(--pc-text-muted)' }}
    >
      <span className="tabular-nums truncate block">
        {segments.map((segment, i) => (
          <Fragment key={i}>
            {i > 0 ? ' · ' : null}
            <span style={segment.faint ? { color: 'var(--pc-text-faint)' } : undefined} title={segment.title}>
              {segment.text}
            </span>
          </Fragment>
        ))}
      </span>
    </div>
  );
}
