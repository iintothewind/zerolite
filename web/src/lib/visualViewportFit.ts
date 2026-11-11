/** Above this the visual viewport is pinch-zoomed: a magnifier, not a keyboard. */
const PINCH_ZOOM_SCALE = 1.01;
/** Layout and visual heights differ by sub-pixel rounding when nothing covers the page. */
const COVERED_MIN_PX = 1;

/**
 * Keep an element on the visual viewport while something outside the page —
 * the on-screen keyboard — covers the bottom of the layout viewport.
 *
 * The keyboard does not resize the layout viewport on iOS Safari, nor on
 * Android Chrome by default: `100dvh` keeps its full height and the browser
 * pans the page to bring the focused field above the keyboard. For a fixed
 * app shell that pan pushes the header off screen and leaves the whole page
 * draggable. Sizing the shell to the visual viewport and moving it to where
 * that viewport sits keeps header, content and composer in the visible strip
 * without fighting the browser's own pan, so overlays outside the shell
 * (modals, the drawer) keep the native behaviour.
 *
 * Inline styles exist only while the viewport is covered; everywhere else —
 * desktop included — the stylesheet stays the single source of the size. A
 * pinch-zoomed visual viewport is a magnifier, not a keyboard, and is left
 * alone.
 *
 * Returns the cleanup: detaches the listeners and clears the styles.
 */
export function fitToVisualViewport(el: HTMLElement): () => void {
  const vv = window.visualViewport;
  if (!vv) return () => {};

  // Height last applied; 0 while uncovered.
  let fitted = 0;
  const apply = () => {
    const covered =
      vv.scale <= PINCH_ZOOM_SCALE &&
      document.documentElement.clientHeight - vv.height > COVERED_MIN_PX;
    el.style.height = covered ? `${vv.height}px` : '';
    // Elastic overscroll reports a negative offset.
    el.style.marginTop = covered ? `${Math.max(0, vv.pageTop)}px` : '';

    const height = covered ? vv.height : 0;
    if (height === fitted) return;
    fitted = height;
    // The pane holding the focused field just got shorter, and the shift above
    // undid the browser's own reveal. Only on a size change: a pan must not
    // drag a reader who scrolled elsewhere back to the field.
    const active = document.activeElement;
    if (covered && active instanceof HTMLElement && el.contains(active)) {
      active.scrollIntoView({ block: 'nearest' });
    }
  };

  vv.addEventListener('resize', apply);
  vv.addEventListener('scroll', apply);
  apply();
  return () => {
    vv.removeEventListener('resize', apply);
    vv.removeEventListener('scroll', apply);
    el.style.height = '';
    el.style.marginTop = '';
  };
}
