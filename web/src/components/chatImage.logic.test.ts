import assert from 'node:assert/strict';
import test from 'node:test';

import { chatImageLocksBubbleWidth, hasChatImage, stepsHaveChatImage } from './chatImage.logic.ts';

test('a tall screenshot would collapse to a sliver under a short cap, so it keeps the bubble width', () => {
  // 1080×8000 at a 480px bubble: a 400px height cap yields a 54px-wide strip.
  assert.equal(chatImageLocksBubbleWidth(480, 1080, 8000), true);
});

test('a landscape image already fits under the cap, so it stays intrinsic', () => {
  assert.equal(chatImageLocksBubbleWidth(480, 1920, 1080), false);
});

test('a square image stays a square instead of being stretched to the bubble width', () => {
  assert.equal(chatImageLocksBubbleWidth(500, 1000, 1000), false);
});

test('missing dimensions do not lock the bubble width', () => {
  assert.equal(chatImageLocksBubbleWidth(0, 1080, 8000), false);
  assert.equal(chatImageLocksBubbleWidth(480, 0, 8000), false);
  assert.equal(chatImageLocksBubbleWidth(480, 1080, 0), false);
});

test('a markdown image marker, or a literal <img>, makes the bubble image-bearing', () => {
  assert.equal(hasChatImage('看 ![图说](/files/a.png) 这张图'), true);
  assert.equal(hasChatImage('<p><img src="a.png" alt="a"></p>'), true);
});

test('text without an image does not widen the bubble', () => {
  assert.equal(hasChatImage('看到图请挂代理'), false);
  assert.equal(hasChatImage('![no closing paren]'), false);
  assert.equal(hasChatImage('a <imgx="1">'), false);
});

test('an image inside the collapsed trajectory counts as image-bearing too', () => {
  const steps = [{ thinking: '', text: '截图已保存', toolCalls: [] }, { thinking: '', text: '![s](/f.png)', toolCalls: [] }];
  assert.equal(stepsHaveChatImage(steps), true);
  assert.equal(stepsHaveChatImage([{ thinking: '', text: '截图已保存', toolCalls: [] }]), false);
  assert.equal(stepsHaveChatImage([]), false);
});
