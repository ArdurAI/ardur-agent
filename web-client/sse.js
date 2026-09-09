// SSE helpers for the Ardur PWA chat client.
//
// The server emits fused-runtime frames:
//   data: {"type":"content","text":"..."}
//   data: {"type":"tool_call_delta","id":"...","delta":"..."}
//   data: {"type":"error","error":"..."}
//   data: {"type":"finish","reason":"..."}
//
// Only `type === "content"` text belongs in the transcript. Tool deltas must
// not be concatenated — they are argument fragments, not assistant prose.

export function splitSse(buffer, chunk) {
  const next = `${buffer}${chunk}`;
  const events = next.split('\n\n');
  const rest = events.pop() || '';
  const payloads = [];
  for (const event of events) {
    const data = event
      .split('\n')
      .filter((line) => line.startsWith('data:'))
      .map((line) => line.slice(5).trimStart())
      .join('\n');
    if (!data || data === '[DONE]') continue;
    payloads.push(JSON.parse(data));
  }
  return { payloads, buffer: rest };
}

export function contentTextFromPayload(parsed) {
  if (parsed && parsed.type === 'error') {
    const message = parsed.error ? String(parsed.error) : 'stream error';
    throw new Error(message);
  }
  if (parsed && parsed.type === 'content' && typeof parsed.text === 'string') {
    return parsed.text;
  }
  return '';
}
