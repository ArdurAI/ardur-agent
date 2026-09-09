import { test } from 'node:test';
import assert from 'node:assert/strict';
import { splitSse, contentTextFromPayload } from './sse.js';

function transcriptFromSse(text) {
  const { payloads } = splitSse('', text.endsWith('\n\n') ? text : `${text}\n\n`);
  return payloads.map((payload) => contentTextFromPayload(payload)).join('');
}

test('content frames concatenate into the assistant transcript', () => {
  const body = [
    'data: {"type":"stage_start","stage":"provider_stream"}',
    '',
    'data: {"type":"content","text":"Hello "}',
    '',
    'data: {"type":"content","text":"world"}',
    '',
    'data: {"type":"finish","reason":"stop"}',
    '',
    '',
  ].join('\n');
  assert.equal(transcriptFromSse(body), 'Hello world');
});

test('tool_call_delta.delta is not appended as assistant text', () => {
  const body = [
    'data: {"type":"content","text":"ok"}',
    '',
    'data: {"type":"tool_call_delta","id":"c1","delta":"{\\"cmd\\":\\"rm\\"}"}',
    '',
    'data: {"type":"receipt","receipt_id":"r1","cost_cents":0}',
    '',
    '',
  ].join('\n');
  assert.equal(transcriptFromSse(body), 'ok');
});

test('in-band error events throw', () => {
  assert.throws(
    () => contentTextFromPayload({ type: 'error', error: 'cost-gate denied' }),
    /cost-gate denied/,
  );
});

test('splitSse keeps a trailing partial frame in the buffer', () => {
  const first = splitSse('', 'data: {"type":"content","text":"He');
  assert.deepEqual(first.payloads, []);
  assert.equal(first.buffer, 'data: {"type":"content","text":"He');
  const second = splitSse(first.buffer, 'llo"}\n\n');
  assert.deepEqual(second.payloads, [{ type: 'content', text: 'Hello' }]);
  assert.equal(second.buffer, '');
});
