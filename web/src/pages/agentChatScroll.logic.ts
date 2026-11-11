/**
 * Follow-the-tail decision for the WebChat transcript pane.
 *
 * A streaming reply appends text many times per second, and the pane should
 * keep the newest output visible while the reader is already at the tail. The
 * hard part is not the following, it is knowing when to stop: the pane used to
 * re-scroll on every streamed update regardless of where the reader was, so
 * anyone who scrolled up to read history was dragged back down before they
 * could finish a sentence (issue #9562).
 *
 * Reader intent is inferred from the *direction* of a scroll, never from the
 * mere existence of a scroll event:
 *
 * - A backwards move (scrollTop decreasing) is deliberate. Only a person can
 *   move the pane away from the tail, so it suspends following immediately.
 *   The exception is a clamp: when the transcript itself gets shorter (an
 *   image settling under its max height, a collapsed group) or the pane gets
 *   taller (the on-screen keyboard closing) the browser pulls scrollTop down
 *   with it. That is still the tail, not a person leaving it.
 * - Returning near the bottom resumes following.
 * - Anything else leaves the decision untouched.
 *
 * This keeps the pane's own tail-following from being mistaken for intent:
 * a programmatic scroll to the tail only ever moves *forward*, so it can never
 * trip the suspend branch, and it needs no "this scroll was mine" bookkeeping
 * that a queued event could outlive.
 */

/** Distance from the bottom, in px, that still counts as being at the tail. */
export const NEAR_BOTTOM_PX = 150;

/** The three scroll geometry values a follow decision needs. */
export interface ScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

export interface FollowState {
  /** True while new output should keep the tail in view. */
  following: boolean;
  /** Scroll offset observed last time, used to detect a backwards move. */
  lastScrollTop: number;
  /**
   * `scrollHeight` and `clientHeight` observed last time. Their difference is
   * the furthest offset the pane can reach; when that drops together with
   * `scrollTop`, the browser clamped the offset — the transcript shrank or
   * the scrollport grew — and the reader did not scroll up.
   */
  lastScrollHeight: number;
  lastClientHeight: number;
}

/**
 * A fresh state that follows the tail. Used on mount and whenever the pane
 * switches conversation, so a reader who had scrolled up in one conversation
 * still lands on the newest message of the next one.
 *
 * Pass the pane's live offset when one is already on screen: seeding
 * `lastScrollTop` with a stale 0 would swallow the reader's first backwards
 * move, because a scroll to 700 reads as forwards when compared against 0.
 */
export function createFollowState(
  lastScrollTop: number = 0,
  lastScrollHeight: number = 0,
  lastClientHeight: number = 0,
): FollowState {
  return { following: true, lastScrollTop, lastScrollHeight, lastClientHeight };
}

/**
 * Re-attach to the tail after an explicit move to it.
 *
 * Submitting from the composer is the one action that should always end at the
 * bottom: the input sits below the transcript, so the reader expects their own
 * message and the reply to land there even if they had been reading history.
 *
 * `lastScrollTop` is deliberately left alone. It already tracks the live
 * offset; resetting it here would swallow the reader's next backwards move.
 */
export function resumeFollowing(state: FollowState): FollowState {
  return { ...state, following: true };
}

/** Pixels of unscrolled content below the current offset. */
export function distanceFromBottom(metrics: ScrollMetrics): number {
  const remaining = metrics.scrollHeight - metrics.clientHeight - metrics.scrollTop;
  // Overscroll (elastic scrolling, a shrunk transcript) reports negative.
  return Math.max(0, remaining);
}

export function isNearBottom(
  metrics: ScrollMetrics,
  threshold: number = NEAR_BOTTOM_PX,
): boolean {
  return distanceFromBottom(metrics) <= threshold;
}

/**
 * Fold one scroll event into the follow state.
 *
 * A backwards move wins over proximity: dragging the scrollbar up while the
 * tail is still within the threshold must suspend, not resume, otherwise the
 * reader can never leave the bottom.
 */
export function foldScroll(
  state: FollowState,
  metrics: ScrollMetrics,
  threshold: number = NEAR_BOTTOM_PX,
): FollowState {
  const observed = {
    lastScrollTop: metrics.scrollTop,
    lastScrollHeight: metrics.scrollHeight,
    lastClientHeight: metrics.clientHeight,
  };
  if (metrics.scrollTop < state.lastScrollTop) {
    // The tail moved up under the reader — the transcript shrank, or the
    // scrollport grew (on-screen keyboard closing, composer collapsing) — and
    // the browser clamped the offset down onto it. They did not scroll away.
    const tailMovedUp =
      metrics.scrollHeight - metrics.clientHeight <
      state.lastScrollHeight - state.lastClientHeight;
    if (tailMovedUp && isNearBottom(metrics, threshold)) {
      return { ...state, ...observed };
    }
    return { following: false, ...observed };
  }
  if (isNearBottom(metrics, threshold)) {
    return { following: true, ...observed };
  }
  // Moving forward but still far from the tail: the reader is scrolling down
  // through history. Leave the decision where it was.
  return { ...state, ...observed };
}

/**
 * Whether a change in the pane's *content* height should move it back to the
 * tail.
 *
 * Content grows on its own, long after the update that introduced it: a
 * markdown image's height is only known once the browser decodes it, and a
 * `<details>` or a collapsed tool group changes height when it is toggled.
 * Neither fires a scroll event, so the one-shot `scrollTop = scrollHeight` in
 * the message effect writes the offset for the pre-image layout and the pane
 * then sits short of the bottom forever — the reader sees the newest output
 * only after scrolling by hand.
 *
 * Re-pin only when the content grew. A shrink (collapsed group, an image
 * settling under its max height) would yank the reader down past text they
 * were reading. And re-pinning is safe only while `following` is still true:
 * a suspended pane must be left exactly where the reader put it, which is the
 * whole point of issue #9562. This function exists so that rule is pinned by
 * a test rather than living only in an observer callback.
 */
export function shouldRepinOnResize(following: boolean, grew: boolean): boolean {
  return following && grew;
}

/**
 * Whether toggling a turn's tool group into `collapsed` should move the pane
 * to the tail.
 *
 * Expanding the group is the reader asking to see more of a turn they are
 * already reading, and it only ever appends below the current offset — so a
 * reader at the tail stays at the tail, which is the reply they just expanded.
 * Collapsing shrinks the content above the offset and would instead yank the
 * pane downwards past text the reader was looking at, so it never re-pins.
 */
export function shouldRepinOnGroupToggle(
  collapsed: boolean,
  following: boolean,
): boolean {
  return !collapsed && following;
}
