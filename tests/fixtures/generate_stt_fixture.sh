#!/usr/bin/env bash
# Generate tests/fixtures/stt_speech.wav for STT bridge/server tests.
#
# Priority:
#   1. TTS_ENDPOINT + TTS_MODEL set → curl POST to TTS server (aivis-speech style)
#   2. macOS with `say` → use macOS built-in TTS (Kyoko voice, ja_JP)
#   3. sox available → create a 2s tone (pipeline only, recognition will be empty)

set -e
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="$SCRIPT_DIR/stt_speech.wav"
TEXT="${TTS_TEXT:-こんにちは、テストです}"

# Option 1: External TTS server
if [[ -n "$TTS_ENDPOINT" && -n "$TTS_MODEL" ]]; then
  ENCODED=$(python3 -c "import urllib.parse; print(urllib.parse.quote('$TEXT'))" 2>/dev/null || echo "%E3%81%93%E3%82%93%E3%81%AB%E3%81%A1%E3%81%AF")
  echo "Generating stt_speech.wav via TTS: $TTS_ENDPOINT (model=$TTS_MODEL)"
  curl -sS -X POST -o "$OUT" \
    "${TTS_ENDPOINT%/}/voice?model=${TTS_MODEL}&text=${ENCODED}&speed=1&format=wav"
  if [[ ! -s "$OUT" ]]; then
    echo "TTS returned empty file" >&2
    exit 1
  fi
  echo "Created $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes) via TTS server"
  exit 0
fi

# Option 2: macOS say (requires sox for format conversion)
if [[ "$(uname)" == "Darwin" ]] && command -v say &>/dev/null && command -v sox &>/dev/null; then
  VOICE="${SAY_VOICE:-Kyoko}"
  AIFF="$SCRIPT_DIR/stt_speech_tmp.aiff"
  echo "Generating stt_speech.wav via macOS say (voice=$VOICE, text=\"$TEXT\")"
  say -v "$VOICE" -o "$AIFF" "$TEXT"
  sox "$AIFF" -r 16000 -c 1 -b 16 "$OUT"
  rm -f "$AIFF"
  echo "Created $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes) via macOS say"
  exit 0
fi

# Option 3: sox tone (no speech recognition expected)
if command -v sox &>/dev/null; then
  echo "No TTS env and no macOS say; creating 2s 440Hz tone WAV with sox."
  echo "WARNING: Recognition tests will fail — use Option 1 or 2 for real speech."
  sox -n -r 16000 -c 1 -b 16 "$OUT" synth 2 sine 440 vol 0.3
  echo "Created $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes) via sox tone"
  exit 0
fi

echo "Cannot generate fixture. Install one of: TTS server, macOS say+sox, or sox." >&2
exit 1
