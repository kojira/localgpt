# Voice test fixtures

## STT server integration test (speech WAV)

Generate `stt_speech.wav` once. For **speech recognition** to return results, use real speech from TTS; a tone WAV only exercises the pipeline.

### Option A: TTS (recommended for recognition)

Set `TTS_ENDPOINT` and `TTS_MODEL` (e.g. aivis-speech), then run:

```bash
cd tests/fixtures
./generate_stt_fixture.sh
```

Optional: `TTS_TEXT=こんにちは` to use different text (default: テスト).

TTS API: **POST** `{endpoint}/voice` with **query** params `model`, `text`, `speed` (optional), `format` (optional).  
See your engine’s `openapi.json` (e.g. `http://<engine>/openapi.json`) for the exact contract.

### Option B: curl (manual)

```bash
curl -X POST -o stt_speech.wav \
  "http://<TTS_ENDPOINT>/voice?model=<model>&text=%E3%83%86%E3%82%B9%E3%83%88&speed=1&format=wav"
```

Replace `<TTS_ENDPOINT>` and `<model>` with your config (e.g. `voice.tts.aivis_speech` in config).

### Option C: sox tone (pipeline only)

With sox installed and no TTS env, `./generate_stt_fixture.sh` creates a 2s tone WAV. The STT bridge test will pass; recognition events may be empty.

### Run STT tests

```bash
# stt-browser-bridge (BlackHole + sox): start bridge first, then:
LOCALGPT_STT_WS_ENDPOINT=ws://127.0.0.1:8765 cargo test --features voice stt_browser_bridge_accepts_fixture -- --include-ignored --nocapture

# Other STT server (e.g. mlx-whisper):
cargo test --features voice -- stt_server_ -- --include-ignored --nocapture
```
