# Discord音声対話システム設計書 v2

**プロジェクト名:** LocalGPT Voice  
**バージョン:** 2.0  
**作成日:** 2026-02-13  
**対象環境:** Mac mini M4 (Apple Silicon, macOS)  
**言語:** Rust

---

## 1. エグゼクティブサマリー

LocalGPTの既存Discord gateway（テキストチャット）に**音声チャンネル対応**を追加する。Discordボイスチャンネルに参加し、ユーザーの発話をSTTで文字起こし→LLMで応答生成→TTSで音声合成→VCへ再生する、リアルタイム音声対話パイプラインを構築する。

### 設計原則

1. **Webサーバーモデル** — メインプロセスはディスパッチャーに徹し、重い処理はワーカーに委譲
2. **trait抽象化** — STT/TTSプロバイダを差し替え可能
3. **config.toml駆動** — すべての設定を外部ファイルで管理
4. **OpenClawに送らない** — 音声処理は独立したカスタムゲートウェイで完結
5. **Rust一本** — メモリ効率とレイテンシ最優先

---

## 2. 既存LocalGPTの構造と統合方針

### 2.1 既存アーキテクチャの要点

LocalGPTは以下のモジュールで構成されるRustプロジェクト:

```
localgpt/src/
├── main.rs          # CLI (clap) → chat/ask/desktop/daemon/memory/config
├── lib.rs           # pub mod agent, config, discord, memory, server, ...
├── agent/           # LLMプロバイダ抽象化・会話管理
├── config/          # Config (TOML) + migration
├── discord/mod.rs   # 自前Discord Gateway (WebSocket直接接続)
├── server/          # HTTP API (axum)
├── memory/          # SQLite + embedding検索
└── desktop/         # egui GUI
```

**Discordモジュールの特徴:**
- `tokio-tungstenite`で直接WebSocket接続（serenityは未使用）
- テキストメッセージのみ処理（`GUILD_MESSAGES` + `MESSAGE_CONTENT` intents）
- チャンネルごとにAgentインスタンスを生成・管理
- メッセージバッチング（3秒間隔で集約）
- `[POST:]`/`[LIST:]`/`[READ:]`/`[REACT:]`タグによるツール実行

### 2.2 統合方針: 独立バイナリ

音声機能は**LocalGPT本体には組み込まない**。理由:

1. **依存関係の分離** — songbird/serenityはLocalGPTの軽量設計と相容れない巨大依存
2. **Discord Voice Gatewayの複雑さ** — 音声はテキストとは別のWebSocket接続・UDP接続を要する
3. **ライフサイクルの違い** — VCの参加/離脱はテキストbotとは独立した状態管理が必要
4. **デプロイの柔軟性** — 音声処理だけリソース制御・再起動したい場合がある

```
┌──────────────────────────────────────────────┐
│              Mac mini M4                      │
│                                               │
│  ┌─────────────┐     ┌────────────────────┐  │
│  │  LocalGPT    │     │  localgpt-voice    │  │
│  │  (daemon)    │     │  (独立バイナリ)      │  │
│  │              │     │                    │  │
│  │  Text GW ◄───┼─────┤  Voice GW          │  │
│  │  HTTP API    │     │  Audio Pipeline    │  │
│  │  Memory      │     │  STT/TTS Workers   │  │
│  └─────────────┘     └────────────────────┘  │
│         │                      │              │
│         ▼                      ▼              │
│  ┌─────────────┐     ┌────────────────────┐  │
│  │ AivisSpeech  │     │ Voxtral/Whisper    │  │
│  │ (TTS Server) │     │ (STT, MLX)         │  │
│  └─────────────┘     └────────────────────┘  │
└──────────────────────────────────────────────┘
```

**LLM連携:** `localgpt-voice`はLocalGPTのHTTP API (`localhost:31327`) を呼び出すか、直接LLMプロバイダに問い合わせる。config.tomlで選択可能。

---

## 3. システムアーキテクチャ

### 3.1 Webサーバーモデル（nginx的ディスパッチャー）

メインスレッドは**一切の重い処理をしない**。受信→ディスパッチ→送信の3つだけ。

```
                    Discord Voice Gateway (WSS + UDP)
                              │
                    ┌─────────▼──────────┐
                    │   Main Dispatcher   │  ← イベントループのみ
                    │   (async, non-blocking)│
                    │                     │
                    │  ┌─── rx queue ───┐ │
                    │  │ user_id, audio │ │
                    │  └───────┬────────┘ │
                    └──────────┼──────────┘
                               │ dispatch
                    ┌──────────▼──────────┐
                    │   Worker Pool        │
                    │                      │
                    │  ┌────────────────┐  │
                    │  │ Pipeline Worker │  │  × N (per-user or pooled)
                    │  │                │  │
                    │  │  STT ─► LLM    │  │
                    │  │         │       │  │
                    │  │        TTS      │  │
                    │  │         │       │  │
                    │  │   tx_audio ────►│──┼──► Main (送信キュー)
                    │  └────────────────┘  │
                    └──────────────────────┘
```

### 3.2 コンポーネント構成

| コンポーネント | 責務 | 実行場所 |
|---|---|---|
| **VoiceGateway** | Discord Voice WSS/UDP接続管理 | Main (songbird) |
| **AudioReceiver** | Opus→PCM変換、VAD、発話区間検出 | Main (軽量) |
| **Dispatcher** | 発話バッファをWorkerに振り分け | Main |
| **PipelineWorker** | STT→LLM→TTSの逐次実行 | Worker (tokio::spawn) |
| **SttProvider** | 音声→テキスト変換 (trait) | Worker内 |
| **LlmBridge** | LLM API呼び出し | Worker内 |
| **TtsProvider** | テキスト→音声変換 (trait) | Worker内 |
| **AudioSender** | PCM→Opus、VCへ再生 | Main (songbird) |
| **ConfigManager** | TOML設定読み込み・検証 | 起動時 |

### 3.3 データフロー

```
[1] User speaks in VC
         │
[2] songbird receives Opus packets (20ms frames)
         │
[3] Opus → PCM (48kHz stereo → 16kHz mono)
         │
[4] VAD: 発話区間検出 (webrtc-vad or silero-vad)
         │ 無音300ms+で区切り
[5] Dispatcher → Worker へ PCM バッファ送信 (mpsc channel)
         │
    ─── Worker内 ───
         │
[6] STT: PCM → テキスト
         │
[7] LLM: テキスト → 応答テキスト (ストリーミング、文単位分割)
         │
[8] TTS: 応答テキスト → PCM (文単位で逐次合成)
         │
[9] Worker → Main へ PCM 送信 (mpsc channel)
         │
    ─── Main ───
         │
[10] PCM → Opus encode → songbird → Discord VC
```

---

## 4. Provider Trait設計

### 4.1 STT Provider

```rust
use async_trait::async_trait;
use anyhow::Result;

/// STT変換結果
pub struct SttResult {
    pub text: String,
    pub language: String,
    pub confidence: f32,
    pub duration_ms: f64,
}

/// STTプロバイダのtrait
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// 音声データをテキストに変換
    /// audio: f32 PCM [-1.0, 1.0], sample_rate: 16000
    async fn transcribe(&self, audio: &[f32]) -> Result<SttResult>;

    /// プロバイダ名
    fn name(&self) -> &str;

    /// リソース解放
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
```

### 4.2 TTS Provider

```rust
/// TTS合成結果
pub struct TtsResult {
    pub audio: Vec<f32>,       // PCM f32
    pub sample_rate: u32,      // e.g. 24000, 44100
    pub duration_ms: f64,
}

/// TTSプロバイダのtrait
#[async_trait]
pub trait TtsProvider: Send + Sync {
    /// テキストを音声に変換
    async fn synthesize(&self, text: &str) -> Result<TtsResult>;

    /// プロバイダ名
    fn name(&self) -> &str;

    /// リソース解放
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
```

### 4.3 LLM Bridge Trait

```rust
/// LLM応答のストリーム (文単位)
pub type SentenceStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

#[async_trait]
pub trait LlmBridge: Send + Sync {
    /// テキスト入力 → 応答テキスト (一括)
    async fn generate(&self, user_id: u64, text: &str) -> Result<String>;

    /// テキスト入力 → 応答テキストストリーム (文単位)
    async fn generate_stream(&self, user_id: u64, text: &str) -> Result<SentenceStream>;

    /// 会話コンテキストリセット
    async fn reset_context(&self, user_id: u64) -> Result<()>;
}
```

---

## 5. デフォルトプロバイダ実装

### 5.1 TTS: AivisSpeech (VOICEVOX互換API)

AivisSpeechはVOICEVOX互換のREST APIを提供するローカルTTSエンジン。

**APIフロー（2ステップ）:**

```
POST /audio_query?text={text}&speaker={style_id}
  → AudioQuery JSON

POST /synthesis?speaker={style_id}
  Body: AudioQuery JSON
  → WAV audio (PCM)
```

**Rust実装概要:**

```rust
pub struct AivisSpeechTts {
    client: reqwest::Client,
    endpoint: String,       // e.g. "http://127.0.0.1:10101"
    style_id: u32,          // 話者スタイルID
}

#[async_trait]
impl TtsProvider for AivisSpeechTts {
    async fn synthesize(&self, text: &str) -> Result<TtsResult> {
        // Step 1: audio_query
        let query: serde_json::Value = self.client
            .post(format!("{}/audio_query", self.endpoint))
            .query(&[("text", text), ("speaker", &self.style_id.to_string())])
            .send().await?
            .json().await?;

        // Step 2: synthesis
        let wav_bytes = self.client
            .post(format!("{}/synthesis", self.endpoint))
            .query(&[("speaker", &self.style_id.to_string())])
            .json(&query)
            .send().await?
            .bytes().await?;

        // WAVヘッダ解析 → PCM f32変換
        let (audio, sample_rate) = decode_wav_to_f32(&wav_bytes)?;
        Ok(TtsResult { audio, sample_rate, duration_ms: /* computed */ })
    }

    fn name(&self) -> &str { "aivis-speech" }
}
```

**config.toml:**

```toml
[tts]
provider = "aivis-speech"

[tts.aivis_speech]
endpoint = "http://127.0.0.1:10101"
style_id = 888753760   # 話者スタイルID
speed_scale = 1.0
```

### 5.2 STT: Voxtral-Mini-3B (MLX) / Whisper

**デフォルト候補: Voxtral-Mini-3B**
- Mistral AIのマルチモーダルモデル、MLXで実行可能
- 日本語対応、3Bパラメータで高精度

**代替: Whisper (mlx-whisper or whisper.cpp)**
- 実績のある音声認識モデル
- Apple Silicon最適化済み

STTプロバイダは**外部プロセス**として実行し、HTTP APIで通信する設計を推奨:

```rust
pub struct HttpSttProvider {
    client: reqwest::Client,
    endpoint: String,     // e.g. "http://127.0.0.1:8766"
}

#[async_trait]
impl SttProvider for HttpSttProvider {
    async fn transcribe(&self, audio: &[f32]) -> Result<SttResult> {
        // PCM → WAV bytes
        let wav = encode_f32_to_wav(audio, 16000)?;

        let resp: SttResponse = self.client
            .post(format!("{}/transcribe", self.endpoint))
            .header("Content-Type", "audio/wav")
            .body(wav)
            .send().await?
            .json().await?;

        Ok(SttResult {
            text: resp.text,
            language: resp.language.unwrap_or("ja".into()),
            confidence: resp.confidence.unwrap_or(1.0),
            duration_ms: resp.duration_ms.unwrap_or(0.0),
        })
    }

    fn name(&self) -> &str { "http-stt" }
}
```

**STTサーバー (Python/MLX、唯一のPython利用箇所):**

MLXモデルの推論はPythonエコシステムが成熟しているため、STTサーバーのみPythonで実装し、HTTPインターフェースでRust側と通信する。これにより:
- Rust本体の純粋性を維持
- MLXモデルの柔軟な切り替え
- プロセス分離によるクラッシュ隔離

```toml
[stt]
provider = "http"

[stt.http]
endpoint = "http://127.0.0.1:8766"
timeout_ms = 10000

# STTサーバー側の設定（別プロセス）
# model = "voxtral-mini-3b"  or "whisper-large-v3"
```

---

## 6. Discord Voice接続

### 6.1 songbird + serenity

Discord Voice Gatewayは複雑（WSS + UDP + 暗号化 + Opus）なため、実績ある`songbird`ライブラリを使用する。

**依存関係:**

```toml
[dependencies]
serenity = { version = "0.12", features = ["voice", "gateway"] }
songbird = { version = "0.4", features = ["driver"] }
```

> **注: serenityの利用範囲は最小限** — Voice Gateway接続のためのみ使用。テキストメッセージ処理はLocalGPT本体が担当するため、`localgpt-voice`はテキストイベントを無視する。

### 6.2 音声受信

```rust
use songbird::events::{Event, EventContext, EventHandler};

struct VoiceReceiveHandler {
    dispatcher_tx: mpsc::Sender<AudioPacket>,
    vad: VadEngine,
    buffers: DashMap<u64, UserAudioBuffer>,  // SSRC → buffer
}

#[async_trait]
impl EventHandler for VoiceReceiveHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoicePacket(packet) = ctx {
            let ssrc = packet.packet.ssrc;
            let pcm = packet.audio.as_ref()?;  // decoded PCM i16

            let pcm_f32: Vec<f32> = pcm.iter()
                .map(|&s| s as f32 / 32768.0)
                .collect();

            // ユーザーバッファに蓄積 + VAD判定
            let mut buf = self.buffers.entry(ssrc).or_default();
            buf.push(&pcm_f32);

            if self.vad.is_speech(&pcm_f32) {
                buf.mark_speech();
            } else {
                buf.mark_silence();
            }

            // 無音区間が閾値超え → 発話完了
            if buf.should_flush() {
                let audio = buf.flush();
                let _ = self.dispatcher_tx.send(AudioPacket {
                    ssrc,
                    audio,
                }).await;
            }
        }
        None
    }
}
```

### 6.3 音声送信

```rust
use songbird::input::{Input, RawAdapter};

async fn play_audio(
    handler: &mut songbird::Call,
    pcm: Vec<f32>,
    sample_rate: u32,
) {
    // f32 → i16
    let pcm_i16: Vec<i16> = pcm.iter()
        .map(|&s| (s * 32767.0).clamp(-32768.0, 32767.0) as i16)
        .collect();

    // リサンプリング (必要時: e.g. 24kHz → 48kHz)
    let resampled = if sample_rate != 48000 {
        resample(&pcm_i16, sample_rate, 48000)
    } else {
        pcm_i16
    };

    // songbird inputとして再生
    let input = RawAdapter::new(resampled, 48000, 2);
    handler.play_input(input.into());
}
```

### 6.4 VC参加コマンド

`localgpt-voice`は独自のDiscord Bot TokenでGatewayに接続する（LocalGPTとは別Bot、または同一Botで`VOICE_STATE`権限を追加）。

VCへの参加方法:
1. **自動参加** — config.tomlで指定したVCに起動時参加
2. **コマンド参加** — テキストチャンネルで`!join`（serenityのイベントハンドラで処理）
3. **API参加** — HTTP APIで制御 (`POST /voice/join?guild=...&channel=...`)

---

## 7. パイプラインWorker

### 7.1 Worker設計

```rust
struct PipelineWorker {
    stt: Arc<dyn SttProvider>,
    llm: Arc<dyn LlmBridge>,
    tts: Arc<dyn TtsProvider>,
}

impl PipelineWorker {
    /// 発話バッファを受け取り、応答音声を返す
    async fn process(
        &self,
        user_id: u64,
        audio: Vec<f32>,
        cancel: CancellationToken,
    ) -> Result<Vec<TtsResult>> {
        // Step 1: STT
        let stt_result = self.stt.transcribe(&audio).await?;
        if stt_result.text.trim().is_empty() {
            return Ok(vec![]);
        }
        tracing::info!(
            user_id, text = %stt_result.text,
            "STT: {}ms", stt_result.duration_ms
        );

        // Step 2: LLM (文単位ストリーム)
        let mut sentence_stream = self.llm
            .generate_stream(user_id, &stt_result.text).await?;

        // Step 3: TTS (文ごとに逐次合成)
        let mut results = Vec::new();
        while let Some(sentence) = sentence_stream.next().await {
            if cancel.is_cancelled() { break; }
            let sentence = sentence?;
            let tts_result = self.tts.synthesize(&sentence).await?;
            results.push(tts_result);
        }

        Ok(results)
    }
}
```

### 7.2 Dispatcher

```rust
struct Dispatcher {
    worker: Arc<PipelineWorker>,
    audio_tx: mpsc::Sender<PlaybackCommand>,  // → Main送信キュー
    active_tasks: DashMap<u64, CancellationToken>,  // SSRC → cancel
}

impl Dispatcher {
    async fn handle(&self, packet: AudioPacket) {
        let ssrc = packet.ssrc;

        // 既存タスクをキャンセル（割り込み）
        if let Some(old) = self.active_tasks.remove(&ssrc) {
            old.1.cancel();
        }

        let cancel = CancellationToken::new();
        self.active_tasks.insert(ssrc, cancel.clone());

        let worker = Arc::clone(&self.worker);
        let tx = self.audio_tx.clone();

        tokio::spawn(async move {
            match worker.process(ssrc as u64, packet.audio, cancel).await {
                Ok(results) => {
                    for tts in results {
                        let _ = tx.send(PlaybackCommand::Play(tts)).await;
                    }
                }
                Err(e) => tracing::error!("Pipeline error: {}", e),
            }
        });
    }
}
```

---

## 8. 設定ファイル (config.toml)

```toml
[discord]
# Bot token (環境変数参照も可)
token = "${DISCORD_VOICE_BOT_TOKEN}"

# 自動参加するVC
[[discord.auto_join]]
guild_id = "1234567890"
channel_id = "9876543210"

[pipeline]
# 割り込み有効化 (ユーザーがBot発話中に話したらキャンセル)
interrupt_enabled = true
# VAD無音判定閾値
silence_threshold_ms = 300
# 最小発話長 (ノイズ除去)
min_speech_ms = 200
# 最大発話長
max_speech_ms = 30000

[stt]
provider = "http"          # "http" | (将来: "whisper-native")

[stt.http]
endpoint = "http://127.0.0.1:8766"
timeout_ms = 10000

[tts]
provider = "aivis-speech"  # "aivis-speech" | (将来: "elevenlabs", "openai")

[tts.aivis_speech]
endpoint = "http://127.0.0.1:10101"
style_id = 888753760
speed_scale = 1.0
pitch_scale = 0.0
intonation_scale = 1.0
volume_scale = 1.0

[llm]
# "localgpt" = LocalGPTのHTTP APIに委譲
# "direct"   = 直接LLMプロバイダに接続
provider = "localgpt"

[llm.localgpt]
endpoint = "http://127.0.0.1:31327"
# LocalGPTのどのチャンネルAgentとして動作するか
channel_id = "voice-default"

[llm.direct]
# provider = "direct" の場合のみ使用
model = "anthropic/claude-sonnet-4-5"
api_key = "${ANTHROPIC_API_KEY}"
system_prompt = "あなたは「のすたろう」です。音声対話なので簡潔に答えてください。"
max_context_turns = 20
context_timeout_sec = 300

[audio]
# 受信サンプルレート (Discord標準)
input_sample_rate = 48000
# STTに渡すサンプルレート
stt_sample_rate = 16000
# VADアグレッシブレベル (0-3)
vad_mode = 3
# プリバッファ (再生開始前に溜めるms)
playback_prebuffer_ms = 100

[server]
# 制御用HTTP API
enabled = true
bind = "127.0.0.1"
port = 31328
```

---

## 9. ディレクトリ構造

```
localgpt-voice/
├── Cargo.toml
├── config.example.toml
├── README.md
├── src/
│   ├── main.rs                 # エントリーポイント (clap CLI)
│   ├── lib.rs
│   ├── config.rs               # Config構造体 (serde + toml)
│   │
│   ├── discord/
│   │   ├── mod.rs              # serenity Client + songbird setup
│   │   ├── handler.rs          # イベントハンドラ (join/leave等)
│   │   └── receiver.rs         # VoiceReceiveHandler (VAD + buffer)
│   │
│   ├── pipeline/
│   │   ├── mod.rs
│   │   ├── dispatcher.rs       # Dispatcher (Main→Worker振り分け)
│   │   ├── worker.rs           # PipelineWorker (STT→LLM→TTS)
│   │   └── audio.rs            # PCM変換・リサンプリングユーティリティ
│   │
│   ├── provider/
│   │   ├── mod.rs              # trait定義 (SttProvider, TtsProvider, LlmBridge)
│   │   ├── stt/
│   │   │   ├── mod.rs
│   │   │   └── http.rs         # HTTP STTプロバイダ
│   │   ├── tts/
│   │   │   ├── mod.rs
│   │   │   └── aivis_speech.rs # AivisSpeech TTSプロバイダ
│   │   └── llm/
│   │       ├── mod.rs
│   │       ├── localgpt.rs     # LocalGPT HTTP API連携
│   │       └── direct.rs       # 直接LLMプロバイダ接続
│   │
│   └── server.rs               # 制御用HTTP API (axum)
│
├── stt-server/                  # STTサーバー (Python/MLX, 別プロセス)
│   ├── pyproject.toml
│   ├── server.py               # FastAPI STTサーバー
│   └── README.md
│
└── tests/
    ├── test_pipeline.rs
    └── test_providers.rs
```

---

## 10. Cargo.toml（主要依存関係）

```toml
[package]
name = "localgpt-voice"
version = "0.1.0"
edition = "2021"

[dependencies]
# Async
tokio = { version = "1", features = ["full"] }
futures = "0.3"
async-trait = "0.1"
tokio-util = "0.7"   # CancellationToken

# Discord + Voice
serenity = { version = "0.12", features = ["voice", "gateway", "cache"] }
songbird = { version = "0.4", features = ["driver"] }

# HTTP
reqwest = { version = "0.12", features = ["json"] }
axum = "0.8"

# Config
serde = { version = "1", features = ["derive"] }
toml = "0.8"
clap = { version = "4", features = ["derive"] }

# Audio processing
rubato = "0.15"         # リサンプリング
hound = "3.5"           # WAV読み書き

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Utilities
anyhow = "1"
dashmap = "6"
serde_json = "1"
```

---

## 11. レイテンシバジェット

ユーザー発話終了から応答音声開始までの目標: **< 2秒**

| ステージ | 目標 | 備考 |
|---|---|---|
| VAD判定 + バッファ送信 | ~50ms | 無音300ms検出後 |
| STT (Voxtral/Whisper) | ~500ms | 5秒間の発話に対して |
| LLM 最初の文 | ~500ms | ストリーミング、TTFT |
| TTS 最初の文 | ~200ms | AivisSpeech, ~20文字 |
| Opus encode + 送信 | ~50ms | |
| **合計** | **~1.3s** | ストリーミングパイプライン時 |

**ストリーミングパイプライン効果:**

```
発話終了 ──► STT(500ms) ──► LLM文1(500ms) ──► TTS文1(200ms) ──► 再生開始
                             LLM文2 ─────────► TTS文2 ──────► キュー
                             LLM文3 ─────────► TTS文3 ──────► キュー
```

文単位で逐次TTS→再生することで、LLM全体の応答完了を待たずに再生開始。

---

## 12. エラーハンドリング

| エラー | リトライ | フォールバック |
|---|---|---|
| STT失敗 | 2回 (100ms間隔) | 「聞き取れませんでした」のTTS再生 |
| STT タイムアウト | 1回 | 同上 |
| LLM API エラー | 3回 (指数バックオフ) | 定型応答「少々お待ちください」 |
| TTS失敗 | 2回 | テキストをログ出力 (音声なし) |
| AivisSpeech接続不可 | 起動時チェック | エラー終了 (必須サービス) |
| Discord VC切断 | 自動再接続 (songbird内蔵) | — |
| STTサーバー切断 | 10秒ごと再試行 | 「音声認識が利用できません」 |

---

## 13. 制御用HTTP API

```
GET  /status                    # 接続状態・現在の設定
POST /voice/join                # VC参加 { guild_id, channel_id }
POST /voice/leave               # VC離脱
POST /voice/interrupt           # 現在の再生を中断
POST /config/reload             # config.toml再読み込み
GET  /metrics                   # レイテンシ統計
```

---

## 14. セキュリティ

- Bot tokenは環境変数 (`${DISCORD_VOICE_BOT_TOKEN}`) で管理
- 制御HTTP APIは`127.0.0.1`バインド（ローカルのみ）
- 音声データはメモリ上のみ、処理後即破棄
- STT/TTSサーバーもローカル通信のみ
- OpenClawのgatewayには一切送信しない

---

## 15. 開発ロードマップ

| Phase | 内容 | 目標 |
|---|---|---|
| **P0** | Cargo骨格 + config.toml + serenity/songbird接続 | VC参加・離脱が動く |
| **P1** | 音声受信 + VAD + AivisSpeech TTS再生 | エコー的な動作確認 |
| **P2** | STTサーバー (Python) + HTTP STTプロバイダ | 音声→テキスト変換 |
| **P3** | LLM Bridge (LocalGPT HTTP API) | 完全パイプライン |
| **P4** | ストリーミングパイプライン + 割り込み | 低レイテンシ化 |
| **P5** | 制御HTTP API + メトリクス | 運用性 |

---

## 16. 前回v1からの主な変更点

| 項目 | v1 | v2 |
|---|---|---|
| 言語 | Python (py-cord) | **Rust** |
| Discord音声 | py-cord voice | **serenity + songbird** |
| アーキテクチャ | モノリシック | **Webサーバーモデル (Dispatcher/Worker分離)** |
| TTS デフォルト | VOICEVOX | **AivisSpeech** (VOICEVOX互換) |
| STT デフォルト | Whisper MLX | **Voxtral-Mini-3B (MLX)** + HTTP API |
| LLM連携 | OpenClaw API直接 | **LocalGPT HTTP API経由** (or 直接) |
| 統合方式 | 独立プロジェクト | **LocalGPTエコシステム内の独立バイナリ** |
| OpenClaw | 送信あり | **送信なし** |
| 設計思想 | プラグイン | **trait抽象化 + config.toml駆動** |

---

*本設計書はLocalGPT Voice v0.1の実装開始にあたっての基盤設計である。実装の進行に伴い更新される。*
