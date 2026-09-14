// 重发恢复会让开头的 role chunk 出现两次（第一轮的 + 第二轮的）。
// 实测客户端能否容忍 —— 这是"零内容可重发"能否成立的前提。
import { createOpenAICompatible } from '@ai-sdk/openai-compatible';
import { createAnthropic } from '@ai-sdk/anthropic';
import { streamText } from 'ai';
import http from 'node:http';

const base = (id) => ({ id, object: 'chat.completion.chunk', created: 1, model: 'm' });
const ch = (id, d, f = null) => ({ ...base(id), choices: [{ index: 0, delta: d, finish_reason: f }] });

function serve(raw, ctype) {
  return new Promise((r) => {
    const s = http.createServer((_q, res) => {
      res.writeHead(200, { 'Content-Type': ctype });
      res.write(raw);
      res.end();
    });
    s.listen(0, '127.0.0.1', () => r({ s, port: s.address().port }));
  });
}

const results = [];

async function openaiCase(label, chunks) {
  const raw = chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join('') + 'data: [DONE]\n\n';
  const { s, port } = await serve(raw, 'text/event-stream');
  const p = createOpenAICompatible({ name: 'p', baseURL: `http://127.0.0.1:${port}/v1` });
  const out = { dialect: 'openai', label, text: '', finish: null, errorPart: false, threw: null, messages: null };
  try {
    const r = streamText({ model: p.chatModel('m'), prompt: 'hi' });
    for await (const part of r.fullStream) {
      if (part.type === 'text-delta') out.text += part.text;
      else if (part.type === 'error') out.errorPart = true;
      else if (part.type === 'finish') out.finish = part.finishReason;
    }
    const resp = await r.response;
    out.messages = resp.messages.map((m) => ({ role: m.role, content: typeof m.content === 'string' ? m.content : JSON.stringify(m.content) }));
  } catch (e) { out.threw = String(e?.message ?? e).slice(0, 120); }
  s.close(); results.push(out); console.log(JSON.stringify(out, null, 1));
}

const MS = (id) => ['message_start', { type: 'message_start', message: { id, type: 'message', role: 'assistant', model: 'm', content: [], stop_reason: null, stop_sequence: null, usage: { input_tokens: 5, output_tokens: 1 } } }];
const an = (records) => records.map(([e, d]) => `event: ${e}\ndata: ${JSON.stringify(d)}\n\n`).join('');

async function anthropicCase(label, records) {
  const { s, port } = await serve(an(records), 'text/event-stream');
  const c = createAnthropic({ apiKey: 'k', baseURL: `http://127.0.0.1:${port}/v1` });
  const out = { dialect: 'anthropic', label, text: '', finish: null, errorPart: false, threw: null, messages: null };
  try {
    const r = streamText({ model: c('m'), prompt: 'hi' });
    for await (const part of r.fullStream) {
      if (part.type === 'text-delta') out.text += part.text;
      else if (part.type === 'error') out.errorPart = true;
      else if (part.type === 'finish') out.finish = part.finishReason;
    }
    const resp = await r.response;
    out.messages = resp.messages.map((m) => ({ role: m.role, content: typeof m.content === 'string' ? m.content : JSON.stringify(m.content).slice(0, 120) }));
  } catch (e) { out.threw = String(e?.message ?? e).slice(0, 120); }
  s.close(); results.push(out); console.log(JSON.stringify(out, null, 1));
}

console.log('### OpenAI：重发留下的重复 role chunk');
await openaiCase('dup-role-then-content', [
  ch('a', { role: 'assistant' }),
  ch('b', { role: 'assistant' }),
  ch('b', { content: 'hello' }),
  ch('b', {}, 'stop'),
]);

console.log('\n### Anthropic：重发留下的重复 message_start + ping');
await anthropicCase('dup-message-start', [
  MS('m1'),
  ['ping', { type: 'ping' }],
  MS('m2'),
  ['ping', { type: 'ping' }],
  ['content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } }],
  ['content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'hello' } }],
  ['content_block_stop', { type: 'content_block_stop', index: 0 }],
  ['message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 3 } }],
  ['message_stop', { type: 'message_stop' }],
]);

import { mkdirSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const HERE = path.dirname(fileURLToPath(import.meta.url));
mkdirSync(path.join(HERE, 'observed'), { recursive: true });
writeFileSync(path.join(HERE, 'observed', 'duplicate-role.json'), JSON.stringify({
  note: '重发恢复产生重复 role/message_start 时，客户端能否容忍。成立则"零内容可安全重发"。',
  results,
}, null, 2));
