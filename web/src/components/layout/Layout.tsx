import { useState, useEffect, useRef } from 'react';
import { Outlet, useLocation } from 'react-router-dom';
import Sidebar from '@/components/layout/Sidebar';
import Header from '@/components/layout/Header';
import ReloadBanner from '@/components/layout/ReloadBanner';
import UnsavedChangesBanner from '@/components/layout/UnsavedChangesBanner';
import CommandPalette, { useCommandPalette } from '@/components/CommandPalette';
import { ErrorBoundary } from '@/App';
import { t } from '@/lib/i18n';
import { fitToVisualViewport } from '@/lib/visualViewportFit';

// First-path-segment → i18n title key, so the browser tab/history/bookmark
// reflects the current page instead of a constant "ZeroClaw".
const TITLE_KEYS: Record<string, string> = {
  agents: 'nav.agents',
  config: 'nav.config',
  setup: 'nav.config',
  tools: 'nav.tools',
  skills: 'nav.skills',
  integrations: 'nav.integrations',
  cron: 'nav.cron',
  logs: 'nav.logs',
  doctor: 'nav.doctor',
  pairing: 'nav.pairing',
  canvas: 'nav.canvas',
  'acp-console': 'nav.acp',
  quickstart: 'nav.quickstart',
  memory: 'nav.memory',
};

export default function Layout() {
  const { pathname } = useLocation();
  const { open: paletteOpen, openPalette, closePalette } = useCommandPalette();
  const [sidebarOpen, setSidebarOpen] = useState(false);

  // Close the mobile drawer on route change.
  useEffect(() => {
    setSidebarOpen(false);
  }, [pathname]);

  // Per-route browser tab title.
  useEffect(() => {
    const seg = pathname.split('/').filter(Boolean);
    const first = seg[0];
    let name: string | null;
    if (!first) {
      name = t('nav.dashboard');
    } else if (first === 'agent' && seg[1]) {
      name = `${decodeURIComponent(seg[1])} · ${t('nav.group.chat')}`;
    } else {
      const key = TITLE_KEYS[first];
      name = key ? t(key) : null;
    }
    document.title = name ? `${name} — ZeroClaw` : 'ZeroClaw';
  }, [pathname]);

  // Keep the shell above the on-screen keyboard (see fitToVisualViewport).
  const shellRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = shellRef.current;
    if (!el) return;
    return fitToVisualViewport(el);
  }, []);

  return (
    // flow-root: the shell's keyboard offset is a margin-top
    // (fitToVisualViewport); without a formatting context of its own that
    // margin collapses through this box and lengthens the document instead
    // of moving the shell.
    <div className="min-h-dvh flow-root bg-pc-base text-pc-text">
      {/* Fixed slim icon rail (desktop) + drawer (mobile). */}
      <Sidebar open={sidebarOpen} onClose={() => setSidebarOpen(false)} />

      {/* Main area — offset by the fixed 56px rail on desktop, full-width on
          mobile. The rail is always slim, so the offset is constant. `dvh`,
          not `vh`: on phones `100vh` is the toolbar-retracted height, which
          made the shell taller than the visible area and the whole page
          scrollable. */}
      <div ref={shellRef} className="flex flex-col flex-1 min-w-0 h-dvh md:ml-14 ml-0">
        <Header
          onMenuToggle={() => setSidebarOpen((v) => !v)}
          onOpenPalette={openPalette}
        />
        <ReloadBanner />
        <UnsavedChangesBanner />

        {/* Page content — ErrorBoundary keyed by the first path segment
            so the boundary resets when the user navigates between pages
            (e.g. /agent → /config), but stays mounted across param-only
            changes within a page (e.g. /config/providers → /config/browser).
            Keying on the full pathname remounted the entire route tree
            on every section click and reset scroll/state. */}
        {/* overflow-x-hidden: overflow-y-auto alone often computes overflow-x
            to auto, which lets a wide child (e.g. chat header) scroll the
            main pane sideways on phones. Body already clips the page; this
            seals the scrollport. */}
        <main className="flex-1 overflow-y-auto overflow-x-hidden min-h-0">
          <ErrorBoundary key={pathname.split('/')[1] ?? ''}>
            <Outlet />
          </ErrorBoundary>
        </main>
      </div>

      {/* Command palette — mounted once for the whole app. Toggled globally
          via ⌘K / Ctrl+K and from the Header search trigger. */}
      <CommandPalette open={paletteOpen} onClose={closePalette} />
    </div>
  );
}
