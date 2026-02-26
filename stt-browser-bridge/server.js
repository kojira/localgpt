#!/usr/bin/env node
/**
 * STT Browser Bridge — WebSocket server that speaks localgpt's STT protocol
 * and forwards audio to a headful browser page running Web Speech API.
 *
 * Protocol (client = localgpt):
 * - Client sends: 1) JSON config, 2) Binary PCM s16le 16kHz mono, 3) Text {"type":"end_of_stream"}
 * - Server sends: config_ack, then speech_start / partial / final / speech_end (JSON)
 *
 * Usage: node server.js [port]
 * Default port: 8765 (or STT_BRIDGE_PORT env)
 * Web Speech API does not work in headless Chrome; use headless: false (or Xvfb on servers).
 */

const WebSocket = require('ws');
const http = require('http');
const path = require('path');
const fs = require('fs');

const PORT = parseInt(process.env.STT_BRIDGE_PORT || process.argv[2] || '8765', 10);
const PAGE_WS_PORT = PORT + 1;

const server = http.createServer();
const wss = new WebSocket.Server({ server, path: '/' });

let browser = null;
let pageServer = null;
let pageWss = null;
const sessions = new Map(); // sessionId -> { clientWs, page, pageWs, config, ... }

/** Real virtual device: captureDeviceLabel for the page; on Linux we create a null-sink and play PCM to it. */
let virtualDeviceLabel = null;
let virtualDeviceCreated = false;
try {
  const vd = require('./virtual-device.js');
  const result = vd.create();
  virtualDeviceLabel = result.captureDeviceLabel;
  virtualDeviceCreated = process.platform === 'linux' || (process.platform === 'darwin' && vd.isPlaybackAvailable());
  console.error('[bridge] Using real virtual device, captureDeviceLabel=' + virtualDeviceLabel + ', playback=' + virtualDeviceCreated);
} catch (e) {
  console.error('[bridge] Real virtual device not available:', e.message);
  virtualDeviceLabel = process.env.STT_BRIDGE_CAPTURE_DEVICE_LABEL || null;
}

const injectScript = fs.readFileSync(path.join(__dirname, 'inject.js'), 'utf8');

function sendToClient(clientWs, obj) {
  if (clientWs && clientWs.readyState === WebSocket.OPEN) {
    clientWs.send(JSON.stringify(obj));
  }
}

async function getBrowser() {
  if (browser) return browser;
  const puppeteer = require('puppeteer');
  const isDocker = process.env.DISPLAY && !process.env.STT_BRIDGE_NO_DOCKER_ARGS;
  const fakeAudioFile = process.env.STT_BRIDGE_FAKE_AUDIO_FILE || null;
  const useFakeDevice = isDocker || !!fakeAudioFile;
  const args = [
    ...(isDocker ? ['--no-sandbox', '--disable-setuid-sandbox'] : []),
    '--autoplay-policy=no-user-gesture-required',
    '--window-size=1,1',
    '--disable-blink-features=AutomationControlled',
    '--use-fake-ui-for-media-stream',
    '--disable-features=AudioServiceSandbox',
    '--unsafely-treat-insecure-origin-as-secure=http://127.0.0.1:' + PAGE_WS_PORT,
    ...(useFakeDevice ? ['--use-fake-device-for-media-stream'] : []),
  ];
  if (fakeAudioFile) {
    args.push('--use-file-for-fake-audio-capture=' + fakeAudioFile);
    console.error('[bridge] Fake audio capture from file: ' + fakeAudioFile);
  }
  if (useFakeDevice) {
    console.error('[bridge] Using fake device for media stream (Docker or STT_BRIDGE_FAKE_AUDIO_FILE)');
  }
  browser = await puppeteer.launch({
    headless: false,
    args,
  });
  const origin = 'http://127.0.0.1:' + PAGE_WS_PORT;
  const ctx = browser.defaultBrowserContext();
  await ctx.overridePermissions(origin, ['microphone']);
  return browser;
}

async function createPageForSession(sessionId) {
  const b = await getBrowser();
  const page = await b.newPage();
  const pageWsUrl = `ws://127.0.0.1:${PAGE_WS_PORT}`;
  await page.evaluateOnNewDocument((sid, url, script) => {
    window.__STT_SESSION_ID__ = sid;
    window.__STT_PAGE_WS_URL__ = url;
    try { (0, eval)(script); } catch (e) { console.error('[bridge inject]', e); }
  }, sessionId, pageWsUrl, injectScript);
  await page.goto('http://127.0.0.1:' + PAGE_WS_PORT + '/', { waitUntil: 'domcontentloaded' });
  await page.mouse.click(1, 1).catch(() => {});
  return page;
}

function flushSessionToPage(session) {
  if (!session.pageWs || session.pageWs.readyState !== WebSocket.OPEN) return;
  if (session.config) {
    const config = { ...session.config };
    config.useRealDevice = virtualDeviceCreated;
    if (virtualDeviceLabel) config.captureDeviceLabel = virtualDeviceLabel;
    console.error('[bridge] sending config to page useRealDevice=' + config.useRealDevice);
    session.pageWs.send(JSON.stringify({ type: 'config', ...config }));
    session.config = null;
  }
  if (session.playback) {
    for (const chunk of session.pcmBuffer) {
      session.playback.write(chunk);
      session.pcmSentToPageSamples += chunk.length / 2;
    }
  } else {
    for (const chunk of session.pcmBuffer) {
      session.pageWs.send(chunk);
      session.pcmSentToPageSamples += chunk.length / 2;
    }
  }
  session.pcmBuffer = [];
  session.pcmBufferLength = 0;
  if (session.pendingEndOfStream) {
    const totalSamples = session.pcmSentToPageSamples;
    const delayMs = Math.max(2000, (totalSamples / 16000) * 1000 + 2000);
    session.pendingEndOfStream = false;
    session.pendingEndDelayMs = null;
    setTimeout(() => {
      if (session.playback) session.playback.end();
      if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
        session.pageWs.send(JSON.stringify({ type: 'end_of_stream' }));
      }
    }, delayMs);
  }
}

wss.on('connection', (clientWs, req) => {
  const sessionId = `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  let firstMessage = true;
  const vd = require('./virtual-device.js');
  const session = {
    clientWs,
    page: null,
    pageWs: null,
    config: null,
    pcmBuffer: [],
    pcmBufferLength: 0,
    pcmSentToPageSamples: 0,
    pendingEndOfStream: false,
    playback: virtualDeviceCreated ? vd.createPlayback() : null,
    recognitionStarted: false,
  };
  sessions.set(sessionId, session);

  createPageForSession(sessionId).then((page) => {
    session.page = page;
  }).catch((err) => {
    console.error('[bridge] createPage failed', err);
    sendToClient(clientWs, { type: 'error', text: String(err.message) });
  });

  clientWs.on('message', (data) => {
    const text = typeof data === 'string' ? data : (Buffer.isBuffer(data) ? data.toString() : null);
    if (firstMessage && text) {
      try {
        const msg = JSON.parse(text);
        if (msg.type === 'config') {
          firstMessage = false;
          session.config = { ...msg };
          session.config.useRealDevice = virtualDeviceCreated;
          if (virtualDeviceLabel) session.config.captureDeviceLabel = virtualDeviceLabel;
          sendToClient(clientWs, { type: 'config_ack' });
          if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
            session.pageWs.send(JSON.stringify({ type: 'config', ...session.config }));
            session.config = null;
          }
          return;
        }
      } catch (e) {}
    }
    if (typeof data === 'string') {
      try {
        const msg = JSON.parse(data);
        if (msg.type === 'end_of_stream') {
          const delayMs = Math.max(2000, (session.pcmSentToPageSamples / 16000) * 1000 + 1500);
          const sendEnd = () => {
            if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
              session.pageWs.send(JSON.stringify({ type: 'end_of_stream' }));
            }
          };
          if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
            setTimeout(sendEnd, delayMs);
          } else {
            session.pendingEndOfStream = true;
            session.pendingEndDelayMs = delayMs;
          }
          return;
        }
      } catch (e) {}
    }
    if (Buffer.isBuffer(data)) {
      const samples = data.length / 2;
      if (session.playback) {
        session.playback.write(data);
        session.pcmSentToPageSamples += samples;
      } else if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
        session.pageWs.send(data);
        session.pcmSentToPageSamples += samples;
      } else {
        session.pcmBuffer.push(data);
        session.pcmBufferLength = (session.pcmBufferLength || 0) + samples;
      }
    }
  });

  clientWs.on('close', () => {
    if (session.playback) session.playback.end();
    if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
      session.pageWs.close();
    }
    if (session.page && !session.page.isClosed()) {
      session.page.close().catch(() => {});
    }
    sessions.delete(sessionId);
  });
});

function startPageWebSocketServer() {
  pageServer = http.createServer((req, res) => {
    if (req.headers.upgrade === 'websocket') return;
    if (req.url && req.url.startsWith('/') && req.method === 'GET') {
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end('<!DOCTYPE html><html><head><meta charset="utf-8"></head><body><button id="stt-start" type="button" style="position:fixed;top:0;left:0;z-index:9999;padding:4px 8px">Start STT</button></body></html>');
      return;
    }
    res.writeHead(404);
    res.end();
  });
  pageWss = new WebSocket.Server({ server: pageServer });
  pageServer.listen(PAGE_WS_PORT, '127.0.0.1', () => {
    console.error(`[bridge] Page WebSocket server on ws://127.0.0.1:${PAGE_WS_PORT}`);
  });

  pageWss.on('connection', (pageWs, req) => {
    const url = new URL(req.url || '', 'http://localhost');
    const sessionId = url.searchParams.get('session');
    if (!sessionId) {
      pageWs.close();
      return;
    }
    const session = sessions.get(sessionId);
    if (!session) {
      pageWs.close();
      return;
    }
    session.pageWs = pageWs;
    flushSessionToPage(session);

    pageWs.on('message', (data) => {
      if (typeof data === 'string') {
        let msg = null;
        try { msg = JSON.parse(data); } catch (e) {}
        if (!msg) {
          if (session.clientWs && session.clientWs.readyState === WebSocket.OPEN) {
            session.clientWs.send(data);
          }
          return;
        }
        if (msg.type === 'debug') {
          console.error('[bridge page]', msg.message != null ? msg.message : JSON.stringify(msg));
          return;
        }
        if (msg.type === 'recognition_started') {
          if (!session.recognitionStarted) {
            session.recognitionStarted = true;
            if (virtualDeviceCreated && !session.playback) {
              const vd = require('./virtual-device.js');
              session.playback = vd.createPlayback();
            }
            console.error('[bridge page] recognition_started: flushing ' + session.pcmBuffer.length + ' buffered chunks');
            for (const chunk of session.pcmBuffer) {
              if (session.playback) {
                session.playback.write(chunk);
              } else if (session.pageWs && session.pageWs.readyState === WebSocket.OPEN) {
                session.pageWs.send(chunk);
              }
              session.pcmSentToPageSamples += chunk.length / 2;
            }
            session.pcmBuffer = [];
            session.pcmBufferLength = 0;
          }
          return;
        }
        if (msg.type === 'use_pcm_fallback') {
          if (session.playback) {
            try { session.playback.end(); } catch (e) {}
            session.playback = null;
          }
          session.recognitionStarted = true;
          console.error('[bridge page] use_pcm_fallback: sending PCM to page');
          return;
        }
        if (msg.type === 'track_live') {
          if (session.page && !session.page.isClosed() && !session.sttStartClicked) {
            session.sttStartClicked = true;
            session.page.click('#stt-start').catch(() => {});
          }
          return;
        }
        if (msg.type === 'final' || msg.type === 'partial') {
          console.error('[bridge page] recognition:', msg.type, msg.text != null ? msg.text.slice(0, 60) : '');
        }
        if (session.clientWs && session.clientWs.readyState === WebSocket.OPEN) {
          session.clientWs.send(data);
        }
      } else if (session.clientWs && session.clientWs.readyState === WebSocket.OPEN) {
        session.clientWs.send(data);
      }
    });

    pageWs.on('close', () => {
      session.pageWs = null;
    });
  });
}

startPageWebSocketServer();

server.listen(PORT, '0.0.0.0', () => {
  console.error(`[bridge] STT WebSocket server on ws://0.0.0.0:${PORT}`);
  console.error(`[bridge] Point localgpt voice.stt.ws.endpoint to ws://127.0.0.1:${PORT}`);
});

process.on('SIGINT', () => {
  try { require('./virtual-device.js').destroy(); } catch (e) {}
  if (browser) browser.close().catch(() => {});
  process.exit(0);
});
