import { Brain, ChevronUp } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import type { EffortLevel } from '@/lib/effort.logic';
import { t } from '@/lib/i18n';
import css from './EffortPicker.module.css';

interface EffortPickerProps {
  /** Levels the current model offers; empty hides the picker. */
  levels: EffortLevel[];
  value: EffortLevel;
  onChange: (level: EffortLevel) => void;
}

/**
 * Composer chip that picks the reasoning effort of the next message: opens a
 * stepped slider with one node per supported level. The popup is positioned
 * against the nearest positioned ancestor (InputBar `.trailing`).
 */
export function EffortPicker({ levels, value, onChange }: EffortPickerProps) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handleClickOutside(e: MouseEvent) {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    }
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') setOpen(false);
    }
    document.addEventListener('mousedown', handleClickOutside);
    document.addEventListener('keydown', handleKeyDown);
    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
      document.removeEventListener('keydown', handleKeyDown);
    };
  }, [open]);

  if (levels.length === 0) return null;

  const label = t('agent.effort.label');
  const index = Math.max(0, levels.indexOf(value));
  const last = levels.length - 1;
  const percent = (i: number) => (last === 0 ? 0 : (i / last) * 100);

  return (
    <div ref={rootRef}>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1 h-7 px-2 rounded-[var(--radius-md)] text-[11px] font-mono text-pc-text-secondary transition-colors hover:text-pc-text hover:bg-[var(--pc-hover)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]"
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-label={label}
        title={label}
      >
        <Brain className="h-3 w-3 shrink-0" />
        <span>{value}</span>
        <ChevronUp className="h-2.5 w-2.5 shrink-0" />
      </button>

      {open && (
        <div
          role="dialog"
          aria-label={label}
          className="absolute bottom-full right-0 mb-1.5 z-50 w-[280px] max-w-[calc(100vw-24px)] rounded-[var(--radius-md)] border border-pc-border bg-pc-elevated px-3 pt-2.5 pb-3 shadow-[var(--pc-shadow-md)]"
        >
          <div className="flex items-baseline justify-between gap-2 text-[11px]">
            <span className="text-pc-text-muted">{label}</span>
            <span className="font-mono text-pc-accent">{value}</span>
          </div>

          <div className="relative mt-2 h-10">
            <input
              type="range"
              min={0}
              max={last}
              step={1}
              value={index}
              onChange={(e) => {
                const next = levels[Number(e.target.value)];
                if (next) onChange(next);
              }}
              aria-label={label}
              aria-valuetext={value}
              className={`peer ${css.range}`}
            />
            {/* Drawn track; the 8px inset is half the native thumb width. */}
            <div className="pointer-events-none absolute inset-x-2 top-2 h-0">
              <div className="absolute inset-x-0 top-0 h-0.5 -translate-y-1/2 rounded-full bg-[var(--pc-text-faint)] opacity-40" />
              <div
                className="absolute left-0 top-0 h-0.5 -translate-y-1/2 rounded-full bg-pc-accent"
                style={{ width: `${percent(index)}%` }}
              />
              {levels.map((level, i) => (
                <span key={level} className="absolute top-0" style={{ left: `${percent(i)}%` }}>
                  <span
                    className={`absolute h-1.5 w-1.5 -translate-x-1/2 -translate-y-1/2 rounded-full ${
                      i <= index ? 'bg-pc-accent' : 'bg-[var(--pc-text-faint)]'
                    }`}
                  />
                  <span
                    className={`absolute top-3 -translate-x-1/2 font-mono text-[10px] leading-none ${
                      i === index ? 'text-pc-accent' : 'text-pc-text-muted'
                    }`}
                  >
                    {level}
                  </span>
                </span>
              ))}
            </div>
            <span
              className="pointer-events-none absolute top-2 h-3.5 w-3.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-pc-accent shadow-[0_0_0_3px_var(--pc-bg-elevated)] peer-focus-visible:ring-2 peer-focus-visible:ring-[var(--pc-focus)]"
              style={{ left: `calc(8px + (100% - 16px) * ${percent(index) / 100})` }}
            />
          </div>

          <p className="text-[11px] leading-snug text-pc-text-secondary">
            {t(`agent.effort.desc.${value}`)}
          </p>
        </div>
      )}
    </div>
  );
}

export default EffortPicker;
