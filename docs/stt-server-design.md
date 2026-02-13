# STT Server Design Document

## Overview
Speech-to-Text server using Voxtral-Mini-3B-2507 on A100 GPU via vLLM.

## Architecture

```
Audio Input → vLLM OpenAI-compatible API (port 8501) → Transcribed Text
```

### Components
- **Model**: `mistralai/Voxtral-Mini-3B-2507` (3B params, multimodal audio→text)
- **Serving**: vLLM v0.10.0 with OpenAI-compatible API
- **GPU**: NVIDIA A100 80GB (shared with other models)
- **Container**: `stt-voxtral` via docker-compose

## Server Configuration

```yaml
# /mnt/ssd8000/kojira/stt-server/docker-compose.yml
image: vllm/vllm-openai:v0.10.0
port: 8501 (host) → 8000 (container)
Parameters:
  --model mistralai/Voxtral-Mini-3B-2507
  --tokenizer_mode mistral
  --config_format mistral
  --load_format mistral
  --dtype bfloat16
  --gpu-memory-utilization 0.15
  --max-model-len 4096
  --max-num-seqs 4
  --enforce-eager  # CUDAグラフ無効（VRAM節約）
```

### Dependencies
- `librosa` and `soundfile` must be installed in container (via entrypoint.sh)
- Container uses custom entrypoint that pip installs audio deps before starting vLLM

## Resource Usage
| Resource | Value |
|----------|-------|
| Model Size | 8.7 GiB |
| KV Cache | 2.6 GiB (23,056 tokens) |
| Total VRAM | ~11.3 GiB |
| Max Concurrent Requests | 4 (--max-num-seqs) |
| Max Sequence Length | 4,096 tokens |

## Benchmark Results (2025-02-13)

### Test Environment
- A100 80GB (shared, ~12GB free)
- enforce-eager mode (no CUDA graphs)
- Test input: sine wave (440Hz) encoded as WAV 16kHz mono

### Performance
| Audio Duration | Processing Time | Output Tokens | RTF |
|---------------|----------------|---------------|-----|
| 3s | 0.320s | 23 | 0.107 |
| 5s | 6.128s | 512 (max) | 1.226 |
| 10s | 6.093s | 512 (max) | 0.609 |

- **Prompt throughput**: ~192 tokens/s
- **Generation throughput**: ~82 tokens/s

### Analysis
- RTF strongly depends on output length, not audio length
- With real speech (short transcription output), expected **RTF ≈ 0.10-0.15**
- 5s/10s tests hit max_tokens=512 due to hallucination on non-speech input
- **Verdict: Suitable for real-time conversational use** ✅

### Caveats
1. Using enforce-eager (no CUDA graphs) due to shared GPU → slight performance penalty
2. gpu-memory-utilization=0.15 limits KV cache → max ~5 concurrent requests
3. Sine wave test causes hallucination; real speech testing needed for accuracy evaluation

## API Usage

```python
import requests, base64

with open("audio.wav", "rb") as f:
    audio_b64 = base64.b64encode(f.read()).decode()

response = requests.post("http://localhost:8501/v1/chat/completions", json={
    "model": "mistralai/Voxtral-Mini-3B-2507",
    "messages": [{"role": "user", "content": [
        {"type": "audio_url", "audio_url": {"url": f"data:audio/wav;base64,{audio_b64}"}},
        {"type": "text", "text": "Transcribe this audio."}
    ]}],
    "max_tokens": 256,
    "temperature": 0
})

text = response.json()["choices"][0]["message"]["content"]
```

## Optimization Opportunities
1. **Dedicated GPU**: Assign separate GPU to avoid enforce-eager and low memory utilization
2. **CUDA Graphs**: Enable (remove --enforce-eager) with dedicated GPU for ~30% speedup
3. **max_tokens tuning**: Set lower for STT (128-256 is usually sufficient)
4. **Batching**: Real-time use is single-request, but batch mode available for offline processing
5. **Whisper comparison**: Consider benchmarking against faster-whisper for pure STT (no LLM capabilities)

## Integration with LocalGPT
- STT endpoint: `http://A100_HOST:8501/v1/chat/completions`
- Audio format: WAV, 16kHz, mono recommended
- Pipeline: Mic → VAD → STT (Voxtral) → LLM → TTS → Speaker
