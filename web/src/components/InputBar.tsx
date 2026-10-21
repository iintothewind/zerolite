import { Loader2, Plus, Send, Square } from 'lucide-react';
import {
  type ChangeEvent,
  type CompositionEvent,
  type KeyboardEvent,
  type RefObject,
} from 'react';
import ContextRing from '@/components/ContextRing';
import SessionStatsRow from '@/components/SessionStatsRow';
import { t } from '@/lib/i18n';
import type { LiveStats } from '@/lib/sessionStats.logic';
import css from './InputBar.module.css';

interface InputBarProps {
  connected: boolean;
  hydrated: boolean;
  typing: boolean;
  uploading: boolean;
  input: string;
  inputRef: RefObject<HTMLTextAreaElement | null>;
  fileInputRef: RefObject<HTMLInputElement | null>;
  contextInputTokens: number | null;
  /** The effective context window: the meter denominator. */
  contextEffectiveWindow: number | null;
  /** The water-line: where proactive trimming engages. A trigger, not a limit. */
  contextWaterlineTokens: number | null;
  liveStats: LiveStats;
  onInputChange: (e: ChangeEvent<HTMLTextAreaElement>) => void;
  onKeyDown: (e: KeyboardEvent<HTMLTextAreaElement>) => void;
  onCompositionStart: (e: CompositionEvent<HTMLTextAreaElement>) => void;
  onCompositionEnd: (e: CompositionEvent<HTMLTextAreaElement>) => void;
  onSend: () => void;
  onAbort: () => void;
  onUpload: (files: Iterable<File>) => Promise<void>;
}

/**
 * Composer card: textarea + toolbar (`+`, status/stats, context ring, send).
 * Visual style mirrors deepseek-harness InputBar; behaviour stays in the parent.
 */
export function InputBar(props: InputBarProps) {
  const {
    connected,
    hydrated,
    typing,
    uploading,
    input,
    inputRef,
    fileInputRef,
    contextInputTokens,
    contextEffectiveWindow,
    contextWaterlineTokens,
    liveStats,
    onInputChange,
    onKeyDown,
    onCompositionStart,
    onCompositionEnd,
    onSend,
    onAbort,
    onUpload,
  } = props;

  const placeholder = !connected
    ? t('agent.connecting')
    : !hydrated
      ? t('agent.session_loading')
      : typing
        ? t('agent.running')
        : t('agent.type_message');

  const sendDisabled = !connected || !hydrated || !input.trim();

  const statusTitle = typing
    ? t('agent.running')
    : connected
      ? t('agent.connected_status')
      : t('agent.disconnected_status');

  return (
    <div className={css.root}>
      <div className={css.card}>
        <input
          ref={fileInputRef}
          type="file"
          accept="image/*"
          multiple
          className="hidden"
          onChange={(e) => {
            if (e.target.files) {
              void onUpload(e.target.files);
            }
            e.target.value = '';
          }}
        />
        <div className={css.scroll}>
          <textarea
            ref={inputRef}
            rows={1}
            value={input}
            onChange={onInputChange}
            onKeyDown={onKeyDown}
            onCompositionStart={onCompositionStart}
            onCompositionEnd={onCompositionEnd}
            placeholder={placeholder}
            disabled={!connected || typing || !hydrated}
            className={css.input}
            style={{ minHeight: '40px', maxHeight: '200px' }}
          />
        </div>
        <div className={css.row}>
          <div className={css.tools}>
            <button
              type="button"
              onClick={() => fileInputRef.current?.click()}
              disabled={uploading}
              className={css.attach}
              aria-label={t('agent.attach_image')}
              title={t('agent.attach_image')}
            >
              {uploading
                ? <Loader2 className="animate-spin" strokeWidth={2} />
                : <Plus strokeWidth={2} />}
            </button>
          </div>
          <div className={css.mid}>
            <span
              className={`status-dot shrink-0 ${css.statusDot}`}
              title={statusTitle}
              style={typing
                ? { background: 'var(--pc-accent)', boxShadow: '0 0 6px var(--pc-accent)' }
                : connected
                  ? { background: 'var(--color-status-success)', boxShadow: '0 0 6px var(--color-status-success)' }
                  : { background: 'var(--color-status-error)', boxShadow: '0 0 6px var(--color-status-error)' }
              }
            />
            <SessionStatsRow stats={liveStats} compact />
          </div>
          <div className={css.trailing}>
            <ContextRing
              used={contextInputTokens}
              max={contextEffectiveWindow}
              waterline={contextWaterlineTokens}
            />
            {typing ? (
              <button
                type="button"
                onClick={onAbort}
                className={css.stop}
                aria-label={t('agent.stop')}
                title={t('agent.stop')}
              >
                <Square fill="currentColor" strokeWidth={2} />
              </button>
            ) : (
              <button
                type="button"
                onClick={onSend}
                disabled={sendDisabled}
                className={css.send}
                aria-label={t('agent.send')}
              >
                <Send strokeWidth={2} />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

export default InputBar;
