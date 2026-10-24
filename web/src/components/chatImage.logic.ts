import type { StepSegment } from '../lib/turnSegments.ts';
/** Height at which a `width: auto` cap would start shrinking a portrait into a
 *  sliver. Images past this take the bubble width instead and keep their ratio. */
export const CHAT_IMAGE_MAX_HEIGHT_PX = 400;

/**
 * A portrait laid out with `width: auto` and a short height cap keeps its ratio
 * by collapsing the width (a 1080×8000 shot becomes ~54px wide). When that
 * would happen, lock the bubble width and let the height follow the ratio so
 * the whole image stays visible. Landscape and near-square images already fit
 * under the cap, so they keep their own size.
 */
export function chatImageLocksBubbleWidth(
  containerWidth: number,
  naturalWidth: number,
  naturalHeight: number,
  maxHeight = CHAT_IMAGE_MAX_HEIGHT_PX,
): boolean {
  if (containerWidth <= 0 || naturalWidth <= 0 || naturalHeight <= 0 || maxHeight <= 0) {
    return false;
  }
  const heightAtFullWidth = (containerWidth * naturalHeight) / naturalWidth;
  if (heightAtFullWidth <= maxHeight) return false;
  const widthAtCap = (maxHeight * naturalWidth) / naturalHeight;
  return widthAtCap < containerWidth * 0.5;
}

/** The markers that make a bubble image-bearing: a markdown `![alt](src)` or a
 *  literal `<img>` in the source. The renderer that sizes the bubble sees the
 *  raw text, so the class has to be decided from it, before any image loads. */
export function hasChatImage(text: string): boolean {
  return /!\[[^\]]*\]\([^)]+\)/.test(text) || /<img\b/i.test(text);
}

/** Steps of the collapsed trajectory whose text carries an image. The group
 *  renders them through the same `img` renderer as the answer, so an image
 *  there needs the same bubble width. */
export function stepsHaveChatImage(steps: StepSegment[]): boolean {
  return steps.some((step) => hasChatImage(step.text));
}
