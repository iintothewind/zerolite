import { useEffect, useState } from 'react';
import { HelpCircle } from 'lucide-react';
import type { PendingQuestion } from '@/types/api';
import { Button } from '@/components/ui';
import { t } from '@/lib/i18n';

interface QuestionBannerProps {
  pending: PendingQuestion;
  /** Questions waiting, this one included. */
  total: number;
  /** `answers: null` dismisses the question. */
  onRespond: (requestId: string, answers: string[] | null) => void;
}

const ROW_CLASS =
  'w-full text-left px-3 py-2 text-sm rounded-[var(--radius-md)] border border-pc-border text-pc-text break-words hover:bg-[var(--pc-hover)] hover:border-pc-border-strong transition-colors cursor-pointer focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]';

export default function QuestionBanner({ pending, total, onRespond }: QuestionBannerProps) {
  const [now, setNow] = useState(() => Date.now());
  const [text, setText] = useState('');
  const [picked, setPicked] = useState<Set<number>>(() => new Set());

  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);

  const remainingSec = pending.timeoutSecs === null
    ? null
    : Math.max(0, Math.ceil(pending.timeoutSecs - (now - pending.receivedAt) / 1000));
  const typed = text.trim();
  const hasChoices = pending.choices.length > 0;
  // Multi-select sends the picked choices in listed order, then the typed text.
  const multiAnswers = pending.multiSelect
    ? [
        ...pending.choices.filter((_, index) => picked.has(index)),
        ...(typed ? [typed] : []),
      ]
    : [];
  const canSubmit = pending.multiSelect ? multiAnswers.length > 0 : typed.length > 0;

  const submit = () => {
    if (!canSubmit) return;
    onRespond(pending.requestId, pending.multiSelect ? multiAnswers : [typed]);
  };

  const toggle = (index: number) => {
    setPicked((current) => {
      const next = new Set(current);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };

  return (
    <div
      role="alert"
      aria-live="assertive"
      aria-labelledby="question-banner-title"
      className="border-b border-pc-border bg-pc-elevated px-4 py-3 animate-fade-in"
    >
      <div className="max-w-4xl mx-auto flex flex-col gap-2">
        <div className="flex items-start gap-3">
          <HelpCircle className="h-5 w-5 shrink-0 mt-0.5 text-pc-accent" />
          <div className="flex-1 min-w-0">
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <p id="question-banner-title" className="text-sm font-semibold text-pc-text">
                {t('agent.question_title')}
                {total > 1 && (
                  <span className="text-xs font-mono text-pc-text-muted"> 1/{total}</span>
                )}
              </p>
              {remainingSec !== null && (
                <span className="text-xs font-mono text-pc-text-muted" aria-hidden="true">
                  {t('agent.question_timeout_in')}: {remainingSec}s
                </span>
              )}
            </div>
            <p className="text-sm mt-1 text-pc-text whitespace-pre-wrap break-words">
              {pending.question}
            </p>
          </div>
        </div>

        {hasChoices && (
          <div className="flex flex-col gap-1.5 max-h-[40dvh] overflow-y-auto">
            {pending.choices.map((choice, index) =>
              pending.multiSelect ? (
                <label
                  key={index}
                  className={`${ROW_CLASS} flex items-start gap-2${picked.has(index) ? ' border-pc-accent' : ''}`}
                >
                  <input
                    type="checkbox"
                    className="mt-0.5 shrink-0"
                    checked={picked.has(index)}
                    onChange={() => toggle(index)}
                  />
                  <span className="min-w-0">{choice}</span>
                </label>
              ) : (
                <button
                  key={index}
                  type="button"
                  className={ROW_CLASS}
                  onClick={() => onRespond(pending.requestId, [choice])}
                >
                  {choice}
                </button>
              ),
            )}
          </div>
        )}

        <div className="flex items-center gap-2 justify-end">
          {pending.freeText && (
            <input
              type="text"
              value={text}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  submit();
                }
              }}
              placeholder={t(
                hasChoices ? 'agent.question_other_placeholder' : 'agent.question_answer_placeholder',
              )}
              className="flex-1 min-w-0 h-9 px-2.5 text-sm rounded-[var(--radius-md)] border border-pc-border bg-pc-surface text-pc-text placeholder:text-pc-text-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]"
            />
          )}
          <Button size="sm" variant="ghost" onClick={() => onRespond(pending.requestId, null)}>
            {t('agent.question_skip')}
          </Button>
          {(pending.multiSelect || pending.freeText) && (
            <Button size="sm" variant="primary" disabled={!canSubmit} onClick={submit}>
              {t(pending.multiSelect ? 'agent.question_submit' : 'agent.question_send')}
            </Button>
          )}
        </div>
      </div>
    </div>
  );
}
