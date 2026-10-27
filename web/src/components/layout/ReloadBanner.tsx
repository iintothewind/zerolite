import { useState } from 'react';
import { AlertTriangle, X } from 'lucide-react';
import { getDrift, type DriftEntry } from '@/lib/api';
import ReloadDaemonButton from '@/components/sections/ReloadDaemonButton';
import { useReloadAvailable } from '@/lib/reloadAvailability';
import { usePolling } from '@/hooks/usePolling';
import { t } from '@/lib/i18n';

const POLL_INTERVAL_MS = 5_000;

interface BannerState {
  drifted: DriftEntry[];
}

/**
 * Layout-level banner for one condition: the on-disk config diverges from what
 * the running daemon loaded, typically because an external editor touched the
 * file. A reload is the only way to pick that up, so the warning is actionable.
 *
 * It deliberately does NOT surface the gateway's `pending_reload` flag. That
 * flag is set by *every* config save (one shared save path in `api_config.rs`),
 * including saves the runtime applies live — switching a session's model writes
 * `agents.<alias>.model_provider` and takes effect on the next turn, yet the old
 * banner answered every such switch with "reload daemon to apply". Asking for a
 * reload that changes nothing is worse than silence; the Config page still
 * exposes the reload action for an operator who really wants it.
 */
export default function ReloadBanner() {
  const [state, setState] = useState<BannerState | null>(null);
  const [pollKey, setPollKey] = useState(0);
  // Signature of the banner content the user last dismissed. The banner
  // re-appears when the drift changes (a new path, or one cleared) because the
  // recomputed signature won't match.
  const [dismissedSig, setDismissedSig] = useState<string | null>(null);
  // Whether the in-UI reload action can actually succeed from this origin.
  // When false (remote host, pairing off) we drop the dead button and reword
  // the notice to point the operator at the CLI / a loopback session.
  const reloadAvailable = useReloadAvailable();

  // Poll only while the tab is visible (no background churn); re-arm on reload.
  usePolling(
    async (isStale) => {
      try {
        const { drifted } = await getDrift();
        if (!isStale()) setState({ drifted });
      } catch {
        // Network blip or auth lapse: keep the prior state.
      }
    },
    POLL_INTERVAL_MS,
    [pollKey],
  );

  if (!state || state.drifted.length === 0) return null;

  const { drifted } = state;
  const driftedCount = drifted.length;
  // Dismissal is keyed to the exact drift set, so nothing else can resurrect
  // the banner.
  const sig = drifted
    .map((d) => d.path)
    .sort()
    .join(',');
  if (dismissedSig === sig) return null;
  return (
    <div className="px-4 py-3 border-b border-status-warning/25 bg-status-warning/[0.06] flex flex-col gap-2 sm:flex-row sm:items-center sm:gap-3">
      <div className="flex items-start gap-3 min-w-0 flex-1">
        <AlertTriangle className="h-4 w-4 flex-shrink-0 mt-0.5 text-status-warning" />
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium text-pc-text break-words">
            {`${driftedCount} ${driftedCount === 1 ? t('reload_banner.path_singular') : t('reload_banner.path_plural')} ${t('reload_banner.differ_suffix')}`}
          </p>
          <ul className="text-xs mt-1 flex flex-col gap-0.5 text-pc-text-muted">
            {drifted.slice(0, 4).map((d) => (
              <li key={d.path} className="font-mono break-all">
                {d.path}
                {d.secret && (
                  <span className="text-pc-text-faint">
                    {' '}
                    {t('reload_banner.secret_label')}
                  </span>
                )}
              </li>
            ))}
            {driftedCount > 4 && (
              <li className="text-pc-text-faint">
                {t('reload_banner.and_more_prefix')}
                {driftedCount - 4}
                {t('reload_banner.and_more_suffix')}
              </li>
            )}
          </ul>
          {!reloadAvailable && (
            <p className="text-xs mt-1 text-pc-text-muted break-words">
              {t('reload_banner.remote_note_prefix')}{' '}
              <code className="font-mono">zeroclaw reload</code>
              {t('reload_banner.remote_note_suffix')}
            </p>
          )}
        </div>
      </div>
      <div className="flex items-center gap-2 shrink-0 self-end sm:self-center">
        {reloadAvailable && (
          <ReloadDaemonButton onReloaded={() => setPollKey((k) => k + 1)} />
        )}
        <button
          type="button"
          onClick={() => setDismissedSig(sig)}
          aria-label={t('reload_banner.dismiss')}
          title={t('reload_banner.dismiss')}
          className="flex-shrink-0 p-1 rounded-[var(--radius-sm)] text-pc-text-muted transition-colors hover:bg-[var(--pc-hover)] hover:text-pc-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)] focus-visible:ring-offset-2 focus-visible:ring-offset-pc-base"
        >
          <X className="h-4 w-4" />
        </button>
      </div>
    </div>
  );
}
