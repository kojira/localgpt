# STT Browser Bridge

WebSocket server that speaks localgpt's STT protocol and uses the browser's **Web Speech API** (Chrome) for recognition. Audio from localgpt is fed into a **real OS-level virtual audio device**; the browser captures from that device with normal `getUserMedia` (no `enumerateDevices`/`getUserMedia` override).

## How it works

- **Linux:** At startup the bridge creates a PulseAudio **null-sink** (name `stt_bridge`, 16 kHz mono). It plays received PCM to that sink via `paplay`. The sink's **monitor** appears as a capture device (e.g. "STT Bridge"); the page uses `getUserMedia` with that device and runs `SpeechRecognition` on the stream. **Recognition works with this real device.**
- **macOS (BlackHole):** If **sox** is installed, the bridge plays received PCM to the **default output device** via `sox`. Install **BlackHole 2ch**, set the system default output to BlackHole, and set Chrome's microphone to BlackHole; the page uses `getUserMedia` with that device and runs `SpeechRecognition`. **Recognition works with this real device.** For recognition on macOS, use sox and BlackHole (see "macOS: BlackHole で認識させる手順" below). Without sox, the bridge falls back to sending PCM to the page (synthetic track); Chrome often returns **`not-allowed`** for that path, so install sox and use BlackHole for reliable recognition.

## Requirements

- Node.js 18+
- Chrome/Chromium (Puppeteer). **Web Speech API does not work in headless mode**; the bridge launches Chrome with a visible window (or use Xvfb on headless servers).
- **Linux:** PulseAudio and `pactl`/`paplay` in PATH. The bridge runs `pactl load-module module-null-sink` at startup and unloads it on exit.
- **macOS (for recognition):** [BlackHole 2ch](https://github.com/ExistentialAudio/BlackHole), **sox** (`brew install sox`), and **`STT_BRIDGE_PLAYBACK_DEVICE=BlackHole 2ch`** so that PCM is played to BlackHole (otherwise sox plays to default output). Optional: set `STT_BRIDGE_CAPTURE_DEVICE_LABEL` to the exact label Chrome shows for the microphone (default is `BlackHole 2ch`).

## Install

```bash
npm install
```

## Run

```bash
npm start
# or: node server.js [port]
# Default port: 8765 (or set STT_BRIDGE_PORT)
```

### macOS: BlackHole で認識させる手順

1. **BlackHole 2ch をインストール**  
   https://github.com/ExistentialAudio/BlackHole からダウンロードしてインストール。
2. **sox をインストール**  
   `brew install sox`
3. **システムの出力を BlackHole 2ch に変更**  
   システム設定 → サウンド → 出力 で「BlackHole 2ch」を選択。  
   （必要な場合は「マルチ出力デバイス」で BlackHole とスピーカーをまとめて使う。）
4. **ブリッジを起動**（認識させる場合のみ環境変数付き）  
   ```bash
   STT_BRIDGE_PLAYBACK_DEVICE="BlackHole 2ch" npm start
   ```  
   起動ログに `playback=true` と出ていれば sox で再生している。BlackHole を渡さない場合は `npm start` のみでよい（既定出力に再生されるが認識は期待しない）。
5. **Chrome のマイクを BlackHole にする**  
   ブリッジが開く Chrome で、マイク許可を出したあと、サイト設定でマイクを「BlackHole 2ch」に変更。  
   または、システムの入力に BlackHole が表示される環境なら、そのデバイスを選ぶ。
6. ラベルが「BlackHole 2ch」でない場合  
   環境変数で `STT_BRIDGE_CAPTURE_DEVICE_LABEL` に Chrome に表示されている名前を指定する。

**音声認識まで動かす検証（macOS）**

1. 上記のとおり BlackHole 2ch と sox を入れ、システム出力を BlackHole にしておく。
2. **テスト用音声**: 認識結果（partial/final）を得るには**実際の音声**の WAV が必要。LocalGPT リポジトリの `tests/fixtures/` で `TTS_ENDPOINT` と `TTS_MODEL` を設定して `./generate_stt_fixture.sh` を実行し、`stt_speech.wav` を生成する（詳細は `tests/fixtures/README.md`）。
3. ブリッジを起動:  
   `STT_BRIDGE_PLAYBACK_DEVICE="BlackHole 2ch" npm start`
4. 別ターミナルでテスト:  
   `LOCALGPT_STT_WS_ENDPOINT=ws://127.0.0.1:8765 cargo test --features voice stt_browser_bridge_accepts_fixture -- --include-ignored --nocapture`  
5. テスト出力の「total events received」が 1 以上、かつ `final` や `partial` が出ていれば認識まで動いている。Chrome のマイクが BlackHole になっていないと「Requested device not found」になるので、サイト設定でマイクを BlackHole 2ch にすること。

Then set localgpt's config:

```toml
[voice.stt]
provider = "ws"

[voice.stt.ws]
endpoint = "ws://127.0.0.1:8765"
```

## Protocol

- **Client (localgpt) sends:** 1) JSON config (`type`, `sample_rate`, `language`, etc.), 2) Binary PCM s16le 16 kHz mono, 3) Text `{"type":"end_of_stream"}`.
- **Server sends:** `config_ack`, then `speech_start` / `partial` / `final` / `speech_end` (JSON).

## Docker (Linux + PulseAudio)

On macOS or when you don't have PulseAudio on the host, run the bridge in Docker. The image provides Linux, PulseAudio (null-sink), Xvfb, and Chromium so recognition works the same as native Linux.

**Prerequisites:** Docker, enough disk space for the image.

```bash
# Build (from stt-browser-bridge/)
docker build -t stt-browser-bridge .

# Run (expose 8765 for localgpt, 8766 for page WebSocket)
docker run --rm -p 8765:8765 -p 8766:8766 stt-browser-bridge
```

Then from the localgpt repo root:

```bash
LOCALGPT_STT_WS_ENDPOINT=ws://127.0.0.1:8765 cargo test --features voice stt_browser_bridge_accepts_fixture -- --include-ignored --nocapture
```

If the test reports **total events received ≥ 1** and you see `partial`/`final` in the output, recognition is working.

## Server without a display

On a headless server, use a virtual display so Chrome can run in headful mode:

```bash
Xvfb :99 -screen 0 1024x768x24 &
DISPLAY=:99 node server.js
```

## Concurrency

Each localgpt STT session opens one browser page. Limit concurrent sessions with localgpt's `voice.stt.max_concurrent_sessions` so the bridge does not open too many tabs.

## Testing

1. Start the bridge: `npm start` (default port 8765).
2. From the localgpt repo root, run the bridge acceptance test:

```bash
LOCALGPT_STT_WS_ENDPOINT=ws://127.0.0.1:8765 cargo test --features voice stt_browser_bridge_accepts_fixture -- --include-ignored --nocapture
```

The test connects, sends config + fixture PCM + end_of_stream, and asserts no error. **On Linux with PulseAudio** or **macOS with sox + BlackHole** (default output = BlackHole, Chrome mic = BlackHole), PCM is played to the real device and the page captures from it; you should see recognition events when the setup is valid. On macOS, use sox and BlackHole to get recognition; without that setup, the PCM fallback may get `not-allowed` from Chrome and the test will pass with 0 recognition events until you complete the BlackHole setup.

To run the **recognition** tests against the bridge, use **Linux**, PulseAudio, and Chrome with Web Speech API enabled. Use `stt_browser_bridge_accepts_fixture` to verify the protocol when recognition is not available.
