import type { ReactNode, Ref } from 'react';
import { RelayNotice } from '@/components/auth/RelayNotice';

/**
 * The dashboard's column beside the rail. It is exactly the window's height
 * and only its `<main>` scrolls, so the page itself never does. The relay
 * notice is its first row: it takes part of that height and stays on screen.
 * Above the column it would make the page taller than the window, and any
 * scroll, such as chat jumping to its latest message, would carry it away.
 */
export function MainColumn({
  viaRelay,
  shellRef,
  children,
}: {
  viaRelay: boolean;
  /** ZeroLite (webui #9): the shell `fitToVisualViewport` keeps above the keyboard. */
  shellRef?: Ref<HTMLDivElement>;
  children: ReactNode;
}) {
  // `dvh`: the visible viewport height, so the column never outgrows what a
  // phone shows with its toolbars out (webui #9; see Layout.tsx).
  return (
    <div ref={shellRef} className="flex flex-col flex-1 min-w-0 h-dvh md:ml-14 ml-0">
      {viaRelay && <RelayNotice bar />}
      {children}
    </div>
  );
}
