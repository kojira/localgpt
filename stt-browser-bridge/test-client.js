#!/usr/bin/env node
/** One-off: connect to bridge, send config, log first message. */
const WebSocket = require('ws');
const url = process.argv[2] || 'ws://127.0.0.1:8769';
const ws = new WebSocket(url);
ws.on('open', () => {
  const config = { type: 'config', sample_rate: 16000, channels: 1, encoding: 'pcm_s16le', language: 'ja', interim_results: true, temperature: 0 };
  ws.send(JSON.stringify(config));
  console.error('[test-client] sent config');
});
ws.on('message', (data) => {
  console.error('[test-client] recv:', data.toString());
  ws.close();
});
ws.on('error', (e) => console.error('[test-client] error', e));
