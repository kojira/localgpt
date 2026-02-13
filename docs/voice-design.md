# Discord音声対話システム設計書 v3

**プロジェクト名:** LocalGPT Voice
**バージョン:** 3.2
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
4. **Rust一本** — メモリ効率とレイテンシ最優先
5. **本体統合** — `src/voice/` モジュールとしてLocalGPT本体に組み込み、Agent/Memoryに直接アクセス
6. **feature flag** — `voice` featureで音声依存を切り替え、不要時はゼロコスト

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

### 2.2 統合方針: 本体モジュール

音声機能は**LocalGPT本体の `src/voice/` モジュール**として統合する。

**統合の利点:**

1. **Agent/Memory直接アクセス** — HTTP API経由不要、`Arc<Agent>` を直接共有してLLM呼び出し・Memory検索が可能
2. **設定の一元管理** — 既存の `config.toml` に `[voice]` セクションを追加するだけ
3. **feature flagによる分離** — `voice` featureが無効なら音声関連の依存は一切コンパイルされない
4. **デプロイの簡素化** — 単一バイナリで完結

**songbirdの直接利用:**
Discord Voice Gatewayは複雑（WSS + UDP + 暗号化 + Opus）なため、`songbird`ライブラリを使用する。serenityは不要で、songbirdのstandalone driver機能を直接使用する。

```
┌───────────────────────────────────────────────┐
│               Mac mini M4                      │
│                                                │
│  ┌──────────────────────────────────────────┐  │
│  │           LocalGPT (daemon)              │  │
│  │                                          │  │
│  │  ┌─────────┐  ┌──────────┐  ┌────────┐  │  │
│  │  │ Text GW │  │ Voice GW │  │ Agent  │  │  │
│  │  │(discord)│  │(songbird)│  │(LLM)   │  │  │
│  │  └─────────┘  └────┬─────┘  └───┬────┘  │  │
│  │                     │            │       │  │
│  │  ┌─────────────────┐│  ┌────────┐│       │  │
│  │  │  Voice Pipeline ◄┘  │ Memory ├┘       │  │
│  │  │  STT → LLM → TTS│  │(SQLite)│        │  │
│  │  └──────────────────┘  └────────┘        │  │
│  └──────────────────────────────────────────┘  │
│                     │                          │
│          ┌──────────┴───────┐                  │
│          ▼                  ▼                  │
│   ┌────────────┐    ┌──────────┐               │
│   │AivisSpeech │    │ Whisper/ │               │
│   │(TTS Server)│    │ Voxtral  │               │
│   │            │    │(STT,MLX) │               │
│   └────────────┘    └──────────┘               │
└────────────────────────────────────────────────┘
```

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
                    │  │  STT ─► Agent  │  │  ← Agent/Memory直接アクセス
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
| **VoiceGateway** | Discord Voice WSS/UDP接続管理 | Main (songbird standalone) |
| **AudioReceiver** | Opus→PCM変換、Workerへ転送 | Main (軽量) |
| **Dispatcher** | PCMチャンクをユーザー別Workerにルーティング | Main |
| **PipelineWorker** | STTストリーミング＋Agent→TTS逐次実行 | Worker (tokio::spawn) |
| **SttProvider** | 音声→テキスト変換 (WebSocket streaming trait) | Worker内 |
| **AgentBridge** | Agent/Memoryへの直接アクセス | Worker内 |
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
[4] Dispatcher → ユーザー別Worker へ PCMチャンク転送 (mpsc channel, 継続的)
         │
    ─── Worker内 (ユーザーごとに常駐) ───
         │
[5] STT: PCMチャンクをWebSocketでSTTサーバーに継続送信
         │ サーバー側VADが発話区間を検出し、認識結果をイベントで返却
         │ SttEvent::Final 受信で確定テキスト取得
         │
[6] Agent: テキスト → 応答テキスト (直接呼び出し、ストリーミング、文単位分割)
         │
[7] TTS: 応答テキスト → PCM (文単位で逐次合成)
         │
[8] Worker → Main へ PCM 送信 (mpsc channel)
         │
    ─── Main ───
         │
[9] PCM → Opus encode → songbird → Discord VC
```

---

## 4. Provider Trait設計

### 4.1 STT Provider（WebSocketストリーミング）

STTはRESTではなく**WebSocketストリーミング**で通信する。クライアント（LocalGPT）はPCMチャンクをWebSocketで連続送信し、サーバーが**VAD（発話区間検出）と音声認識を一体で**実行する。パイプライン側にVADは持たない。

```rust
use async_trait::async_trait;
use anyhow::Result;
use serde::Deserialize;

/// STTサーバーから受信するイベント
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SttEvent {
    /// 発話開始検出（サーバー側VAD）
    #[serde(rename = "speech_start")]
    SpeechStart { timestamp_ms: u64 },

    /// 中間認識結果（未確定、随時上書き更新される）
    #[serde(rename = "partial")]
    Partial { text: String },

    /// 確定認識結果（発話区間ごとの最終結果）
    #[serde(rename = "final")]
    Final {
        text: String,
        language: String,
        confidence: f32,
        duration_ms: f64,
    },

    /// 発話終了検出（サーバー側VAD）
    #[serde(rename = "speech_end")]
    SpeechEnd { timestamp_ms: u64, duration_ms: f64 },
}

/// STTストリーミングセッション
/// WebSocket接続1本を表し、PCM送信とイベント受信を行う
#[async_trait]
pub trait SttSession: Send {
    /// PCMデータチャンクを送信（16kHz mono f32 LE）
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()>;

    /// 次のイベントを受信（None = セッション終了）
    async fn recv_event(&mut self) -> Result<Option<SttEvent>>;

    /// セッションを閉じる
    async fn close(&mut self) -> Result<()>;
}

/// STTプロバイダのtrait（WebSocketストリーミング）
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// 新しいストリーミングセッションを開始（WebSocket接続確立）
    async fn connect(&self) -> Result<Box<dyn SttSession>>;

    /// プロバイダ名
    fn name(&self) -> &str;

    /// リソース解放
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
```

#### 4.1.1 STT WebSocketプロトコル

**クライアント→サーバー（Binary frame）:**

PCM f32 LE のバイト列をそのまま送信。フレームサイズは任意（推奨: 20ms = 320 samples × 4 bytes = 1280 bytes）。

**サーバー→クライアント（Text frame / JSON）:**

```json
// 発話開始（サーバー側VADが音声検知）
{ "type": "speech_start", "timestamp_ms": 12340 }

// 中間認識結果（発話中に随時送信、textは最新状態で上書き）
{ "type": "partial", "text": "こんに" }
{ "type": "partial", "text": "こんにちは" }

// 確定認識結果（発話区間の最終結果）
{
  "type": "final",
  "text": "こんにちは",
  "language": "ja",
  "confidence": 0.95,
  "duration_ms": 1200.0
}

// 発話終了（サーバー側VADが無音検知）
{ "type": "speech_end", "timestamp_ms": 13540, "duration_ms": 1200.0 }
```

**イベントの発生順序:**

```
speech_start → partial* → final → speech_end
```

- `partial` は0回以上（短い発話では省略される場合あり）
- `final` は `speech_end` の直前に必ず1回送信される
- 1つのWebSocket接続で複数の発話区間を連続処理できる

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

#### 4.2.1 TTS直接Opus出力の検討と48kHz制約

Discord Voice Gatewayは48kHz Opusを要求する。現在のパイプラインは TTS→PCM→リサンプル→Opus encode だが、TTSが直接Opus/48kHzを出力できればこの工程を省略できる。しかし以下の懸念がある:

- **サンプルレート不一致**: 多くのTTSエンジン（AivisSpeech含む）は24kHz/44.1kHzで生成。48kHz Opus直接出力に対応するエンジンは限定的
- **品質劣化リスク**: TTS内部で48kHzにアップサンプルしてからOpusエンコードすると、無駄な帯域拡張やアーティファクト発生の可能性がある
- **Opus設定の整合性**: Discord側のOpus設定（ビットレート、フレームサイズ20ms固定）との整合が必要。TTS側で独自にOpusエンコードすると不一致が起きうる

**現時点の方針:** PCM出力→Rust側でリサンプル（rubato）→Opus encodeの従来パイプラインを維持する。TTSエンジンのOpus直接出力対応が成熟した段階で再検討する。

### 4.3 Agent直接アクセス

LlmBridge traitは不要。LocalGPT本体のAgent/Memoryモジュールに直接アクセスする。

```rust
use crate::agent::Agent;
use crate::memory::MemoryStore;
use std::sync::Arc;
use futures::Stream;
use std::pin::Pin;

/// 文単位のストリーム
pub type SentenceStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// Voice用のAgent連携
/// Agent/Memoryを直接保持し、HTTP API経由せずに呼び出す
pub struct VoiceAgentBridge {
    agent: Arc<Agent>,
    memory: Arc<MemoryStore>,
}

impl VoiceAgentBridge {
    pub fn new(agent: Arc<Agent>, memory: Arc<MemoryStore>) -> Self {
        Self { agent, memory }
    }

    /// テキスト入力 → 応答テキスト (一括)
    pub async fn generate(&self, user_id: u64, text: &str) -> Result<String> {
        // Agentの既存メソッドを直接呼び出し
        // チャンネルID = "voice-{user_id}" として会話コンテキストを管理
        let channel_id = format!("voice-{}", user_id);
        self.agent.chat(&channel_id, text).await
    }

    /// テキスト入力 → 応答テキストストリーム (文単位)
    pub async fn generate_stream(
        &self,
        user_id: u64,
        text: &str,
    ) -> Result<SentenceStream> {
        let channel_id = format!("voice-{}", user_id);
        // Agentのストリーミングレスポンスを文単位に分割
        let stream = self.agent.chat_stream(&channel_id, text).await?;
        Ok(split_into_sentences(stream))
    }

    /// 会話コンテキストリセット
    pub async fn reset_context(&self, user_id: u64) -> Result<()> {
        let channel_id = format!("voice-{}", user_id);
        self.agent.reset(&channel_id).await
    }

    /// Memory検索 (音声コンテキストの補強に使用)
    pub async fn search_memory(&self, query: &str, limit: usize) -> Result<Vec<String>> {
        self.memory.search(query, limit).await
    }
}
```

**直接アクセスの利点:**
- HTTP往復のレイテンシ (~1-5ms) を完全に排除
- Agent内部の会話コンテキスト・人格設定をそのまま利用
- Memory検索をLLM呼び出し前に行い、コンテキスト補強が可能
- エラーハンドリングがRustの型システムで統一

---

## 5. プロバイダ実装方針

本設計書ではSTT/TTSプロバイダの**trait定義のみ**（セクション4参照）を規定する。各プロバイダの具体的な実装詳細は、実装フェーズにおいて別ドキュメントとして整備する。

### 5.1 想定プロバイダ

| カテゴリ | プロバイダ候補 | 通信方式 | 備考 |
|---|---|---|---|
| **STT** | Whisper (mlx-whisper) | WebSocket | ローカル実行、Apple Silicon最適化 |
| **STT** | Voxtral-Mini-3B (MLX) | WebSocket | ローカル実行、日本語高精度 |
| **TTS** | AivisSpeech (VOICEVOX互換) | REST API | ローカル実行、日本語特化 |
| **TTS** | OpenAI TTS | REST API | クラウド、低レイテンシ |

### 5.2 STTサーバー構成

STTプロバイダは**外部プロセス**（Python/MLX）として実行し、WebSocketで通信する:

- WebSocketエンドポイントでPCMストリーム（16kHz mono f32 LE）を受信
- サーバー側でVAD（発話区間検出）＋音声認識を一体で実行
- 認識結果を `SttEvent` JSON（セクション4.1.1参照）でクライアントに返却
- プロセス分離によりMLXモデルのクラッシュがLocalGPT本体に影響しない

MLXモデルの推論はPythonエコシステムが成熟しているため、STTサーバーのみPythonで実装する。Rust側は `SttProvider` traitのWebSocket実装を通じて通信する。

### 5.3 TTSサーバー構成

TTSプロバイダはREST APIで通信する既存サーバー（AivisSpeech等）を利用する。`TtsProvider` traitの実装を通じてHTTPリクエストを発行する。具体的なAPI呼び出しフロー・パラメータは実装ドキュメントで定義する。

---

## 6. Discord Voice接続

### 6.1 songbird standalone driver

Discord Voice Gatewayは複雑（WSS + UDP + 暗号化 + Opus）なため、実績ある`songbird`ライブラリを使用する。serenityは不要で、songbirdのstandalone driver機能を直接利用する。

**依存関係 (feature flag):**

```toml
[features]
default = []
voice = ["songbird", "rubato", "hound", "tokio-tungstenite"]

[dependencies]
# Voice (feature gated)
songbird = { version = "0.4", features = ["driver", "gateway"], optional = true }
rubato = { version = "0.15", optional = true }              # リサンプリング
hound = { version = "3.5", optional = true }                # WAV読み書き
tokio-tungstenite = { version = "0.24", optional = true }   # STT WebSocketクライアント
```

> **注: serenityへの依存は不要。** songbirdのstandalone driver機能を使用し、LocalGPTの既存Discord WebSocket接続からVoice State Update/Voice Server Updateイベントを受け取ってsongbirdに渡す。

#### songbird standalone初期化とBot token共有

LocalGPTの既存Discord Gatewayは`tokio-tungstenite`でWebSocket接続を管理している。songbird standalone driverはserenityに依存せず、`ConnectionInfo`を手動で渡すことでVoice Gateway接続を確立する。Bot tokenは既存のDiscord設定（`config.discord.bot_token`）をそのまま共有する。

```rust
use songbird::{Songbird, id::{GuildId, UserId}};
use std::sync::Arc;

/// VoiceManagerがArc<Songbird>を保持し、既存のGatewayイベントループと共存する
pub struct VoiceManager {
    songbird: Arc<Songbird>,
    bot_user_id: UserId,
    /// Voice State Update受信時に蓄積し、Voice Server Updateで接続を完了する
    pending_voice_states: DashMap<GuildId, VoiceStateData>,
}

impl VoiceManager {
    pub fn new(bot_user_id: u64) -> Self {
        // standalone driver: serenity不要、独立で動作
        let songbird = Songbird::serenity();
        Self {
            songbird: Arc::new(songbird),
            bot_user_id: UserId::from(bot_user_id),
            pending_voice_states: DashMap::new(),
        }
    }

    /// VC参加リクエスト（既存Gateway経由でVoice State Update op=4を送信）
    pub async fn join(
        &self,
        guild_id: u64,
        channel_id: u64,
        gateway_tx: &mpsc::Sender<GatewayCommand>,
    ) -> Result<()> {
        // 既存Discord GatewayにVoice State Update (op=4) 送信を依頼
        gateway_tx.send(GatewayCommand::VoiceStateUpdate {
            guild_id,
            channel_id: Some(channel_id),
            self_mute: false,
            self_deaf: false,
        }).await?;
        Ok(())
    }
}
```

### 6.2 Voice Gateway連携: イベント受け渡しと接続確立

LocalGPTの既存Discord Gateway（`src/discord/`）がVoice関連イベントを受信し、`src/voice/`モジュールに転送する。songbird standalone driverにはVoice State UpdateとVoice Server Updateの2つのイベントを渡す必要があり、両方が揃った時点でsongbirdがVoice Gateway WSS + UDP接続を確立する。

#### イベント受け渡しコード

```rust
// src/discord/mod.rs (既存コードへの追加)
#[cfg(feature = "voice")]
use crate::voice::VoiceManager;

// Gateway イベントハンドラ内
match event {
    // 既存のテキスト処理...

    #[cfg(feature = "voice")]
    GatewayEvent::VoiceStateUpdate(data) => {
        voice_manager.handle_voice_state_update(data).await;
    }
    #[cfg(feature = "voice")]
    GatewayEvent::VoiceServerUpdate(data) => {
        voice_manager.handle_voice_server_update(data).await;
    }
    _ => {}
}
```

#### ConnectionInfo構築とsongbirdへの接続情報渡し

Voice State UpdateとVoice Server Updateの両方を受信した時点で、songbirdの`ConnectionInfo`を構築しドライバーに渡す:

```rust
use songbird::ConnectionInfo;

impl VoiceManager {
    /// Voice State Update受信: session_idを保存
    pub async fn handle_voice_state_update(&self, data: VoiceStateData) {
        // 自分のBot以外のVoice State Updateは無視（他ユーザーの入退室）
        if data.user_id != self.bot_user_id.0 {
            // ただしSSRC→UserIDマッピングの更新には使用（セクション17参照）
            self.update_ssrc_mapping(&data).await;
            return;
        }
        let guild_id = GuildId::from(data.guild_id);
        self.pending_voice_states.insert(guild_id, data);
    }

    /// Voice Server Update受信: endpoint + tokenが揃い接続を開始
    pub async fn handle_voice_server_update(&self, data: VoiceServerData) {
        let guild_id = GuildId::from(data.guild_id);

        // 対応するVoice State Updateが蓄積されているか確認
        let state = match self.pending_voice_states.remove(&guild_id) {
            Some((_, state)) => state,
            None => {
                tracing::warn!("Voice Server Update received without prior Voice State Update");
                return;
            }
        };

        // songbird用のConnectionInfoを構築
        let info = ConnectionInfo {
            channel_id: Some(state.channel_id.into()),
            endpoint: data.endpoint.clone(),
            guild_id: guild_id,
            session_id: state.session_id.clone(),
            token: data.token.clone(),
            user_id: self.bot_user_id,
        };

        // songbird driverに接続情報を渡してVoice Gateway接続を開始
        let handler_lock = self.songbird.get_or_insert(guild_id);
        let mut handler = handler_lock.lock().await;
        if let Err(e) = handler.connect(info).await {
            tracing::error!("Failed to connect to voice: {}", e);
            return;
        }

        // 音声受信ハンドラを登録
        self.register_audio_receiver(&mut handler).await;

        tracing::info!(
            guild_id = guild_id.0,
            "Voice connected via songbird standalone driver"
        );
    }
}
```

#### イベントループ共存

`Arc<Songbird>`はVoiceManagerが保持し、既存のtokio-tungstenite Gatewayイベントループ内で駆動される。songbird内部のドライバータスクは`tokio::spawn`で独自に動作するため、既存のテキストGatewayイベントループをブロックしない:

```
┌──────────────────────────────────────────┐
│        tokio runtime (共有)              │
│                                          │
│  Task 1: Discord Text Gateway            │
│    └─ tokio-tungstenite WebSocket        │
│       └─ match event {                   │
│            VoiceStateUpdate → VoiceMgr   │
│            VoiceServerUpdate → VoiceMgr  │
│            Message → テキスト処理        │
│          }                               │
│                                          │
│  Task 2: songbird driver (内部spawn)     │
│    └─ Voice WSS接続                      │
│    └─ UDP送受信                          │
│    └─ Opus encode/decode                 │
│                                          │
│  Task 3..N: PipelineWorker per user      │
│    └─ STT → Agent → TTS                 │
└──────────────────────────────────────────┘
```

### 6.3 音声受信

VAD（発話区間検出）はSTTサーバー側の責務とし、受信側はOpus→PCM変換とリサンプリングのみ行う。PCMチャンクはDispatcherを経由してユーザー別Workerに継続転送される。

```rust
use songbird::events::{Event, EventContext, EventHandler};

struct AudioChunk {
    ssrc: u32,
    pcm: Vec<f32>,  // 16kHz mono f32
}

struct VoiceReceiveHandler {
    dispatcher_tx: mpsc::Sender<AudioChunk>,
}

#[async_trait]
impl EventHandler for VoiceReceiveHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoicePacket(packet) = ctx {
            let ssrc = packet.packet.ssrc;
            let pcm = packet.audio.as_ref()?;  // decoded PCM i16

            // i16 → f32 変換
            let pcm_f32: Vec<f32> = pcm.iter()
                .map(|&s| s as f32 / 32768.0)
                .collect();

            // 48kHz stereo → 16kHz mono リサンプリング
            let pcm_16k = resample_to_16k_mono(&pcm_f32);

            // Dispatcherへ転送（VADはSTTサーバー側で実行）
            let _ = self.dispatcher_tx.send(AudioChunk {
                ssrc,
                pcm: pcm_16k,
            }).await;
        }
        None
    }
}
```

### 6.4 音声送信

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

### 6.5 VC参加コマンド

VCへの参加方法:
1. **自動参加** — config.tomlで指定したVCに起動時参加
2. **テキストコマンド** — テキストチャンネルで`!join`（既存のDiscord Gatewayハンドラで処理）
3. **API参加** — 既存HTTP APIに追加 (`POST /voice/join?guild=...&channel=...`)

---

## 7. パイプラインWorker

### 7.1 Worker設計

WorkerはユーザーごとにSTT WebSocketセッションを維持し、PCMチャンクを継続送信する。STTサーバーからの `SttEvent::Final` をトリガーにAgent→TTSパイプラインを実行する。

```rust
struct PipelineWorker {
    stt: Arc<dyn SttProvider>,
    agent_bridge: Arc<VoiceAgentBridge>,
    tts: Arc<dyn TtsProvider>,
}

impl PipelineWorker {
    /// ユーザーごとのストリーミングパイプラインを起動
    /// audio_rxからPCMチャンクを受信し続け、STTイベントに応じて処理する
    async fn run_stream(
        &self,
        user_id: u64,
        mut audio_rx: mpsc::Receiver<Vec<f32>>,
        playback_tx: mpsc::Sender<PlaybackCommand>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut session = self.stt.connect().await?;

        loop {
            tokio::select! {
                // PCMチャンクをSTTサーバーへWebSocket送信
                Some(pcm) = audio_rx.recv() => {
                    session.send_audio(&pcm).await?;
                }
                // STTサーバーからイベント受信
                event = session.recv_event() => {
                    match event? {
                        Some(SttEvent::Final { text, .. }) => {
                            if text.trim().is_empty() { continue; }
                            tracing::info!(user_id, %text, "STT final");

                            // Agent → TTS パイプライン
                            self.process_text(
                                user_id, &text, &playback_tx, &cancel
                            ).await?;
                        }
                        Some(SttEvent::Partial { text }) => {
                            tracing::debug!(user_id, %text, "STT partial");
                        }
                        Some(SttEvent::SpeechStart { .. }) => {
                            tracing::debug!(user_id, "Speech start");
                        }
                        Some(SttEvent::SpeechEnd { duration_ms, .. }) => {
                            tracing::debug!(user_id, duration_ms, "Speech end");
                        }
                        None => break, // セッション終了
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }

        session.close().await?;
        Ok(())
    }

    /// 確定テキストからAgent→TTSを実行
    async fn process_text(
        &self,
        user_id: u64,
        text: &str,
        playback_tx: &mpsc::Sender<PlaybackCommand>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Agent (文単位ストリーム) — 直接呼び出し
        let mut sentence_stream = self.agent_bridge
            .generate_stream(user_id, text).await?;

        // TTS (文ごとに逐次合成)
        while let Some(sentence) = sentence_stream.next().await {
            if cancel.is_cancelled() { break; }
            let sentence = sentence?;
            let tts_result = self.tts.synthesize(&sentence).await?;
            let _ = playback_tx.send(PlaybackCommand::Play(tts_result)).await;
        }

        Ok(())
    }
}
```

### 7.2 Dispatcher

DispatcherはPCMチャンクをユーザー別Workerにルーティングする。初回チャンク受信時にWorkerのストリーミングパイプラインを起動し、以降はチャンネル経由で転送する。

```rust
struct Dispatcher {
    worker: Arc<PipelineWorker>,
    playback_tx: mpsc::Sender<PlaybackCommand>,
    /// SSRC → Workerへのaudio送信チャンネル
    user_sessions: DashMap<u32, mpsc::Sender<Vec<f32>>>,
    active_cancels: DashMap<u32, CancellationToken>,
}

impl Dispatcher {
    /// PCMチャンクを適切なWorkerセッションにルーティング
    async fn handle_audio(&self, chunk: AudioChunk) {
        let ssrc = chunk.ssrc;

        // 既存セッションがあれば転送
        if let Some(tx) = self.user_sessions.get(&ssrc) {
            let _ = tx.send(chunk.pcm).await;
            return;
        }

        // 新規ユーザー: ストリーミングパイプラインを起動
        let (audio_tx, audio_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();

        self.user_sessions.insert(ssrc, audio_tx.clone());
        self.active_cancels.insert(ssrc, cancel.clone());

        let worker = Arc::clone(&self.worker);
        let playback_tx = self.playback_tx.clone();
        let sessions = self.user_sessions.clone();

        tokio::spawn(async move {
            if let Err(e) = worker.run_stream(
                ssrc as u64, audio_rx, playback_tx, cancel
            ).await {
                tracing::error!("Pipeline error for SSRC {}: {}", ssrc, e);
            }
            // セッション終了時にクリーンアップ
            sessions.remove(&ssrc);
        });

        // 最初のチャンクを送信
        let _ = audio_tx.send(chunk.pcm).await;
    }

    /// 指定ユーザーのパイプラインをキャンセル（割り込み時）
    fn cancel_user(&self, ssrc: u32) {
        if let Some((_, cancel)) = self.active_cancels.remove(&ssrc) {
            cancel.cancel();
        }
    }
}
```

---

## 8. 設定ファイル (config.toml)

既存の `config.toml` に `[voice]` セクションを追加する形式:

```toml
[voice]
# 音声機能を有効化 (feature="voice"でビルド時のみ有効)
enabled = true

[voice.discord]
# 自動参加するVC
[[voice.discord.auto_join]]
guild_id = "1234567890"
channel_id = "9876543210"

[voice.pipeline]
# 割り込み有効化 (ユーザーがBot発話中に話したらキャンセル)
interrupt_enabled = true

[voice.stt]
provider = "ws"            # "ws" | (将来: "whisper-native")

[voice.stt.ws]
endpoint = "ws://127.0.0.1:8766/ws"
# 再接続設定
reconnect_interval_ms = 1000
max_reconnect_attempts = 10

[voice.tts]
provider = "aivis-speech"  # "aivis-speech" | (将来: "elevenlabs", "openai")

[voice.tts.aivis_speech]
endpoint = "http://127.0.0.1:10101"
style_id = 888753760
speed_scale = 1.0
pitch_scale = 0.0
intonation_scale = 1.0
volume_scale = 1.0

[voice.agent]
# 音声チャンネル用のAgent設定 (省略時は既存Agentの設定を継承)
# voice専用の人格設定を上書きしたい場合に使用
# system_prompt_override = "あなたは音声アシスタントです。簡潔に応答してください。"

[voice.audio]
# 受信サンプルレート (Discord標準)
input_sample_rate = 48000
# STTに渡すサンプルレート
stt_sample_rate = 16000
# プリバッファ (再生開始前に溜めるms)
playback_prebuffer_ms = 100
```

---

## 9. ディレクトリ構造

```
localgpt/src/
├── main.rs                     # CLI: --voice フラグ追加
├── lib.rs                      # #[cfg(feature = "voice")] pub mod voice;
├── agent/                      # 既存 (変更なし)
├── config/                     # VoiceConfig追加
├── discord/
│   ├── mod.rs                  # Voice State/Server Update転送追加
│   └── ...
├── memory/                     # 既存 (変更なし)
├── server/                     # /voice/* エンドポイント追加
│
├── voice/                      # ★ 新規モジュール (feature = "voice")
│   ├── mod.rs                  # VoiceManager: 初期化・ライフサイクル管理
│   ├── gateway.rs              # songbird standalone driver連携
│   ├── receiver.rs             # VoiceReceiveHandler (PCM受信・転送)
│   ├── dispatcher.rs           # Dispatcher (Main→Worker振り分け)
│   ├── worker.rs               # PipelineWorker (STT→Agent→TTS)
│   ├── agent_bridge.rs         # VoiceAgentBridge (Agent/Memory直接アクセス)
│   ├── audio.rs                # PCM変換・リサンプリングユーティリティ
│   └── provider/
│       ├── mod.rs              # trait定義 (SttProvider, TtsProvider)
│       ├── stt/
│       │   ├── mod.rs
│       │   └── ws.rs           # WebSocket STTプロバイダ
│       └── tts/
│           ├── mod.rs
│           └── aivis_speech.rs # AivisSpeech TTSプロバイダ
│
└── stt-server/                  # STTサーバー (Python/MLX, 別プロセス)
    ├── pyproject.toml
    ├── server.py               # WebSocket STTサーバー
    └── README.md
```

---

## 10. Cargo.toml（主要依存関係）

```toml
[package]
name = "localgpt"
version = "0.1.0"
edition = "2021"

[features]
default = []
voice = [
    "dep:songbird",
    "dep:rubato",
    "dep:hound",
    "dep:tokio-tungstenite",
]

[dependencies]
# === 既存の依存関係 ===
tokio = { version = "1", features = ["full"] }
futures = "0.3"
async-trait = "0.1"
tokio-util = "0.7"        # CancellationToken
reqwest = { version = "0.12", features = ["json"] }
axum = "0.8"
serde = { version = "1", features = ["derive"] }
toml = "0.8"
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
anyhow = "1"
dashmap = "6"
serde_json = "1"

# === Voice依存 (feature gated) ===
songbird = { version = "0.4", features = ["driver", "gateway"], optional = true }
rubato = { version = "0.15", optional = true }
hound = { version = "3.5", optional = true }
tokio-tungstenite = { version = "0.24", optional = true }
```

**ビルド:**

```bash
# 音声機能なし（デフォルト）
cargo build

# 音声機能あり
cargo build --features voice
```

---

## 11. レイテンシバジェット

ユーザー発話終了から応答音声開始までの目標: **< 2秒**

| ステージ | 目標 | 備考 |
|---|---|---|
| PCM転送 + STTサーバー側VAD | ~50ms | サーバー側で発話区間検出 |
| STT (Voxtral/Whisper) | ~500ms | 5秒間の発話に対して |
| Agent 最初の文 | ~500ms | 直接呼び出し、ストリーミング、TTFT |
| TTS 最初の文 | ~200ms | AivisSpeech, ~20文字 |
| Opus encode + 送信 | ~50ms | |
| **合計** | **~1.3s** | ストリーミングパイプライン時 |

**直接アクセスによる改善:**
HTTP API経由と比較して、Agent直接呼び出しにより1-5msのレイテンシ削減。さらにMemory検索を事前に実行し、コンテキスト付きでLLMに渡すことで応答品質も向上。

**ストリーミングパイプライン効果:**

```
発話終了 ──► STT(500ms) ──► Agent文1(500ms) ──► TTS文1(200ms) ──► 再生開始
                             Agent文2 ──────────► TTS文2 ──────► キュー
                             Agent文3 ──────────► TTS文3 ──────► キュー
```

文単位で逐次TTS→再生することで、LLM全体の応答完了を待たずに再生開始。

---

## 12. エラーハンドリング

| エラー | リトライ | フォールバック |
|---|---|---|
| STT失敗 | 2回 (100ms間隔) | 「聞き取れませんでした」のTTS再生 |
| STT WebSocket切断 | 自動再接続 (設定値に従う) | 再接続中は「音声認識が一時停止中です」 |
| Agent エラー | 3回 (指数バックオフ) | 定型応答「少々お待ちください」 |
| TTS失敗 | 2回 | テキストをログ出力 (音声なし) |
| AivisSpeech接続不可 | 起動時チェック | エラー終了 (必須サービス) |
| Discord VC切断 | 自動再接続 (songbird内蔵) | — |
| STTサーバー起動不可 | 起動時チェック | エラー終了 (必須サービス) |

---

## 13. 制御用HTTP API

既存のLocalGPT HTTP APIに追加:

```
GET  /voice/status              # 接続状態・現在の設定
POST /voice/join                # VC参加 { guild_id, channel_id }
POST /voice/leave               # VC離脱
POST /voice/interrupt           # 現在の再生を中断
GET  /voice/metrics             # レイテンシ統計
```

---

## 14. セキュリティ

- Bot tokenは環境変数 (`${DISCORD_BOT_TOKEN}`) で管理（テキスト/音声で共有）
- 制御HTTP APIは`127.0.0.1`バインド（ローカルのみ）
- 音声データはメモリ上のみ、処理後即破棄
- STT/TTSサーバーもローカル通信のみ

---

## 15. 開発ロードマップ

| Phase | 内容 | 目標 |
|---|---|---|
| **P0** | `src/voice/` 骨格 + feature flag + config.toml + songbird standalone接続 | VC参加・離脱が動く |
| **P1** | 音声受信 + VAD + AivisSpeech TTS再生 | エコー的な動作確認 |
| **P2** | STTサーバー (Python) + HTTP STTプロバイダ | 音声→テキスト変換 |
| **P3** | VoiceAgentBridge (Agent/Memory直接アクセス) | 完全パイプライン |
| **P4** | ストリーミングパイプライン + 割り込み | 低レイテンシ化 |
| **P5** | 制御HTTP API + メトリクス | 運用性 |

---

*本設計書はLocalGPT Voice v3.2の実装開始にあたっての基盤設計である。実装の進行に伴い更新される。*

## 16. TTS応答パイプライン拡張

本セクションでは、LLM応答の音声合成・再生における低レイテンシ化と自然な対話体験を実現するための拡張設計を記述する。

### 16.1 TTS文分割（Sentence Segmentation）

#### 概要

LLMからのストリーミング応答テキストを文単位でセグメント化し、セグメント単位で逐次TTS生成・再生を行う。応答全文の生成完了を待たずに音声出力を開始できるため、体感レイテンシを大幅に削減する。

#### 分割ルール

```
分割文字: 「。」「！」「？」「\n\n」
```

- LLMストリーミング出力のチャンクを内部バッファに蓄積
- 分割文字を検出した時点でバッファ内容をセグメントとして切り出し
- 各セグメントに連番（`seg_index: 0, 1, 2, ...`）を付与
- ストリーム終了時にバッファに残ったテキストがあれば最終セグメントとして切り出し

#### データ構造

```python
@dataclass
class TTSSegment:
    seg_index: int          # セグメント番号（0始まり）
    text: str               # セグメントテキスト
    request_id: str         # 親リクエストID
    audio_path: Optional[Path] = None   # 生成済み音声ファイルパス
    status: str = "pending" # pending | generating | ready | playing | done | cancelled
```

#### 処理フロー

```
LLM Stream → [バッファ蓄積] → 「。」検出 → Segment生成 → TTSキューへ
                              → 「！」検出 → Segment生成 → TTSキューへ
                              → 「？」検出 → Segment生成 → TTSキューへ
                              → Stream終了 → 残バッファ → TTSキューへ
```

#### split_into_sentences Rust実装

LLMストリーミングレスポンス（トークン単位の`Stream<Item = String>`）を受け取り、句読点で分割して文単位の`Stream<Item = String>`に変換する。セクション4.3の`VoiceAgentBridge::generate_stream`から呼び出される。

```rust
use futures::{Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::mpsc;

/// 句読点リスト（分割トリガー）
const SENTENCE_DELIMITERS: &[char] = &['。', '！', '？', '!', '?'];

/// LLMストリーミング出力を句読点で文分割する
///
/// トークン単位のストリームをバッファに蓄積し、句読点を検出した時点で
/// バッファ内容を1文として切り出す。ストリーム終了時に残りをフラッシュする。
pub fn split_into_sentences(
    token_stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    let (tx, rx) = mpsc::channel::<Result<String>>(32);

    tokio::spawn(async move {
        let mut buffer = String::new();
        let mut stream = token_stream;

        while let Some(token_result) = stream.next().await {
            let token = match token_result {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };

            // トークンをバッファに追加
            buffer.push_str(&token);

            // バッファ内の句読点を検出して分割
            loop {
                let split_pos = buffer.char_indices().find_map(|(i, c)| {
                    if SENTENCE_DELIMITERS.contains(&c) {
                        Some(i + c.len_utf8())
                    } else {
                        None
                    }
                });

                match split_pos {
                    Some(pos) => {
                        let sentence: String = buffer.drain(..pos).collect();
                        let trimmed = sentence.trim().to_string();
                        if !trimmed.is_empty() {
                            if tx.send(Ok(trimmed)).await.is_err() {
                                return; // receiver dropped
                            }
                        }
                    }
                    None => break, // 句読点なし、次のトークンを待つ
                }
            }

            // "\n\n" による分割（段落区切り）
            if buffer.contains("\n\n") {
                let parts: Vec<&str> = buffer.splitn(2, "\n\n").collect();
                let sentence = parts[0].trim().to_string();
                buffer = parts[1].to_string();
                if !sentence.is_empty() {
                    if tx.send(Ok(sentence)).await.is_err() {
                        return;
                    }
                }
            }
        }

        // ストリーム終了: バッファに残ったテキストをフラッシュ
        let remaining = buffer.trim().to_string();
        if !remaining.is_empty() {
            let _ = tx.send(Ok(remaining)).await;
        }
    });

    // mpsc receiverをStreamに変換
    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
}
```

### 16.2 セグメント並列生成＋順序保証再生

#### 概要

全セグメントのTTSリクエストを並列で発行し生成スループットを最大化する一方、再生は必ずセグメント番号順（seg0 → seg1 → seg2 → ...）を保証する。

#### 設計方針

- **並列生成**: セグメント生成時に即座にTTS APIへ非同期リクエストを発行（`asyncio.create_task`）
- **順序保証再生**: `PlaybackOrchestrator` が `next_play_index` を管理し、該当セグメントが `ready` になった時点で再生開始
- **先行完了の待機**: seg2がseg1より先に完了しても、seg1の再生完了まで待機

#### シーケンス図

```
Segmenter          TTS Worker Pool       PlaybackOrchestrator      AudioPlayer
    |                    |                        |                      |
    |-- seg0 生成 ------>|                        |                      |
    |-- seg1 生成 ------>|                        |                      |
    |-- seg2 生成 ------>|                        |                      |
    |                    |                        |                      |
    |                    |-- seg1 完了(先行) ---->| (待機: seg0未完了)    |
    |                    |-- seg0 完了 --------->| next=0, ready!        |
    |                    |                        |-- seg0 再生開始 ---->|
    |                    |                        |                      |-- seg0 再生完了
    |                    |                        |<-- 完了通知 ---------|
    |                    |                        | next=1, seg1 ready!  |
    |                    |                        |-- seg1 再生開始 ---->|
    |                    |-- seg2 完了 --------->|                      |
    |                    |                        |                      |-- seg1 再生完了
    |                    |                        |<-- 完了通知 ---------|
    |                    |                        | next=2, seg2 ready!  |
    |                    |                        |-- seg2 再生開始 ---->|
```

#### 主要コンポーネント

```python
class PlaybackOrchestrator:
    """セグメントの順序保証再生を管理"""

    def __init__(self):
        self.segments: dict[int, TTSSegment] = {}
        self.next_play_index: int = 0
        self._ready_event: asyncio.Event = asyncio.Event()

    async def on_segment_ready(self, segment: TTSSegment):
        """TTSワーカーからのコールバック"""
        self.segments[segment.seg_index] = segment
        segment.status = "ready"
        self._ready_event.set()

    async def playback_loop(self):
        """再生ループ - 番号順に再生"""
        while not self._is_complete():
            await self._ready_event.wait()
            self._ready_event.clear()
            while self.next_play_index in self.segments:
                seg = self.segments[self.next_play_index]
                if seg.status == "ready":
                    seg.status = "playing"
                    await self._play_audio(seg)
                    seg.status = "done"
                    self.next_play_index += 1
```

#### 並列数制御

```toml
# config.toml
[tts.pipeline]
max_concurrent_requests = 3   # 同時TTSリクエスト数上限
segment_timeout_sec = 10      # セグメント単位タイムアウト
```

### 16.3 割り込み（Barge-in / Interrupt）

#### 概要

Bot発話（音声再生）中にVADでユーザー音声を検知した場合、即座に再生を停止し、未再生セグメントを破棄する。会話履歴には再生完了済みセグメントのテキストのみを記録する。

#### 割り込みフロー

```
[Bot再生中: seg0完了, seg1完了, seg2再生中]
        |
        v
VADがユーザー音声検知
        |
        v
1. AudioPlayer.stop() → seg2の再生を即時停止
2. PlaybackOrchestrator.cancel_remaining() → seg3以降を破棄
3. 会話履歴に記録: seg0.text + seg1.text のみ
   （seg2は再生途中のため記録しない）
4. 新しいユーザー入力の処理を開始
```

#### 状態遷移図

```
                          ┌─────────┐
                          │ IDLE    │
                          └────┬────┘
                               │ LLM応答開始
                               v
                          ┌─────────┐
                     ┌───>│BUFFERING│ (セグメント蓄積中)
                     │    └────┬────┘
                     │         │ seg0 ready
                     │         v
                     │    ┌─────────┐
                     │    │PLAYING  │ (音声再生中)
                     │    └────┬────┘
                     │         │
                     │    ┌────┴─────────────┐
                     │    │                  │
                     │    v                  v
                     │ ┌──────┐     ┌────────────┐
                     │ │DONE  │     │INTERRUPTED │
                     │ └──┬───┘     └──────┬─────┘
                     │    │                │
                     │    v                v
                     │  全seg再生完了    再生停止+未再生破棄
                     │    │                │
                     │    v                v
                     └────┴── IDLE ────────┘
```

#### 会話履歴への記録ルール

```python
class InterruptHandler:
    """割り込み処理"""

    async def handle_interrupt(self, orchestrator: PlaybackOrchestrator):
        # 1. 再生停止
        await orchestrator.stop_playback()

        # 2. 再生完了済みセグメントのテキストのみ収集
        completed_text = ""
        for i in range(orchestrator.next_play_index):
            seg = orchestrator.segments.get(i)
            if seg and seg.status == "done":
                completed_text += seg.text

        # 3. 再生中・未再生セグメントを破棄
        orchestrator.cancel_remaining()

        # 4. 会話履歴に部分テキストとして記録
        if completed_text:
            conversation_history.add_assistant_message(
                text=completed_text,
                interrupted=True  # 割り込みフラグ
            )

        return completed_text
```

#### 割り込み時のTTS停止＋LLM生成キャンセル

割り込み検知時は、再生中のTTSを停止するだけでなく、**進行中のLLMストリーミング生成も即座にキャンセル**する。これにより不要なLLM推論コストとTTS合成を排除する。

**CancellationToken伝播経路:**

```
VAD検知 (STTサーバー: speech_start)
    │
    ▼
Dispatcher::handle_interrupt(ssrc)
    │
    ├── 1. PlaybackOrchestrator::stop_playback()  ← TTS再生停止
    │
    ├── 2. active_cancels[ssrc].cancel()          ← CancellationToken発火
    │       │
    │       ▼
    │   PipelineWorker::process_text()
    │       │
    │       ├── agent_bridge.generate_stream() 内の LLMストリーム中断
    │       │     └── tokio::select! { cancel.cancelled() => break }
    │       │
    │       └── tts.synthesize() のキャンセル
    │             └── tokio::select! { cancel.cancelled() => break }
    │
    └── 3. 新しいCancellationTokenを生成して次の発話に備える
```

**Rustコード:**

```rust
use tokio_util::sync::CancellationToken;

impl Dispatcher {
    /// 割り込み処理: TTS停止 + LLM生成キャンセル
    async fn handle_interrupt(&self, ssrc: u32) {
        // 1. 現在のパイプラインをキャンセル（LLMストリーム + TTS合成を中断）
        if let Some((_, old_cancel)) = self.active_cancels.remove(&ssrc) {
            old_cancel.cancel();
        }

        // 2. 再生キューをクリア
        let _ = self.playback_tx.send(PlaybackCommand::Stop).await;

        // 3. 新しいCancellationTokenを発行（次の発話用）
        let new_cancel = CancellationToken::new();
        self.active_cancels.insert(ssrc, new_cancel);

        tracing::info!(ssrc, "Interrupt: cancelled LLM stream and TTS playback");
    }
}

impl PipelineWorker {
    /// CancellationToken対応のAgent→TTSパイプライン
    async fn process_text(
        &self,
        user_id: u64,
        text: &str,
        playback_tx: &mpsc::Sender<PlaybackCommand>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Agent (文単位ストリーム) — CancellationTokenで中断可能
        let mut sentence_stream = self.agent_bridge
            .generate_stream(user_id, text).await?;

        while let Some(sentence) = sentence_stream.next().await {
            // LLMストリーム読み取り中にキャンセルチェック
            if cancel.is_cancelled() {
                tracing::debug!(user_id, "LLM stream cancelled by interrupt");
                break;
            }

            let sentence = sentence?;

            // TTS合成もキャンセル可能にラップ
            let tts_result = tokio::select! {
                result = self.tts.synthesize(&sentence) => result?,
                _ = cancel.cancelled() => {
                    tracing::debug!(user_id, "TTS synthesis cancelled by interrupt");
                    return Ok(());
                }
            };

            let _ = playback_tx.send(PlaybackCommand::Play(tts_result)).await;
        }

        Ok(())
    }
}
```

#### 割り込み判定パラメータ

```toml
# config.toml
[voice.interrupt]
enabled = true
min_speech_duration_ms = 200    # 割り込み判定の最小発話長（誤検知防止）
energy_threshold = 0.02         # VADエネルギー閾値
cooldown_ms = 500               # 割り込み後のクールダウン期間
```

### 16.4 TTSキャッシュ

#### 概要

同一テキスト・同一パラメータの音声合成結果をSQLiteに**BLOBとして直接格納**し、再利用することでTTS APIコールを削減する。音声データはOpus形式でエンコードしてからBLOBカラムに保存し、ファイルシステムへの分散保存を排除してデータ管理を単純化する。

#### DBスキーマ

```sql
CREATE TABLE tts_cache (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    cache_key TEXT NOT NULL UNIQUE,       -- パラメータのSHA256ハッシュ
    text TEXT NOT NULL,                   -- 元テキスト
    model TEXT NOT NULL,                  -- TTSモデル名 (e.g., "voicevox", "style_bert_vits2")
    speed REAL NOT NULL DEFAULT 1.0,      -- 話速
    style_id INTEGER,                     -- スタイルID（モデル依存）
    speaker_id INTEGER,                   -- 話者ID
    pitch REAL,                           -- ピッチ
    audio_format TEXT NOT NULL DEFAULT 'opus',  -- 音声フォーマット (opus)
    audio_data BLOB NOT NULL,             -- 音声データ (Opusエンコード済みバイナリ)
    duration_ms INTEGER,                  -- 音声の長さ（ミリ秒）
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_used_at TEXT NOT NULL DEFAULT (datetime('now')),
    use_count INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX idx_tts_cache_key ON tts_cache(cache_key);
CREATE INDEX idx_tts_cache_last_used ON tts_cache(last_used_at);
```

**ファイル保存方式との比較:**

| 観点 | BLOB格納 (採用) | ファイル保存 |
|------|-----------------|-------------|
| データ一貫性 | SQLiteトランザクションで保証 | DB-ファイル間の不整合リスク |
| バックアップ | DBファイル1つをコピーするだけ | DB + ディレクトリ全体のコピーが必要 |
| 削除・GC | DELETE文のみ（VACUUM不要、auto_vacuum対応） | DBレコード削除 + ファイル削除の2段階 |
| ストレージ効率 | Opusエンコードにより1文あたり数KB〜数十KB | WAV: 数十KB〜数百KB |
| I/O | SQLite内蔵ページキャッシュが効く | ファイルシステムキャッシュ依存 |

#### キャッシュキー生成

```rust
use sha2::{Sha256, Digest};
use serde::Serialize;

#[derive(Serialize)]
struct CacheKeyParams<'a> {
    text: &'a str,
    model: &'a str,
    speed: f64,
    style_id: Option<i64>,
    speaker_id: Option<i64>,
    pitch: Option<f64>,
}

fn generate_cache_key(
    text: &str,
    model: &str,
    speed: f64,
    style_id: Option<i64>,
    speaker_id: Option<i64>,
    pitch: Option<f64>,
) -> String {
    let params = CacheKeyParams {
        text, model, speed, style_id, speaker_id, pitch,
    };
    let canonical = serde_json::to_string(&params).unwrap();
    let hash = Sha256::digest(canonical.as_bytes());
    hex::encode(hash)
}
```

#### キャッシュ設定

```toml
# config.toml
[tts.cache]
enabled = true                          # キャッシュ有効/無効
db_path = "${data_dir}/cache/tts_cache.db"  # SQLite DBパス
max_entries = 10000                     # 最大エントリ数
max_total_size_mb = 500                 # 最大合計サイズ(MB) — length(audio_data)の合計
eviction_policy = "lru"                 # 追い出しポリシー (lru | ttl)
ttl_days = 30                           # TTLベース追い出し時の有効期限
cleanup_interval_hours = 24             # 定期クリーンアップ間隔
```

#### キャッシュ利用フロー

```
TTSリクエスト
    |
    v
キャッシュキー生成 (text + model + speed + style_id + ...)
    |
    v
SQLite SELECT (cache_key)
    |
    ├── HIT → audio_data BLOB を直接読み出し → Opusデコード → PCM返却
    │         UPDATE last_used_at, use_count
    │
    └── MISS → TTS API呼び出し → PCM取得
               → PCM → Opusエンコード
               → INSERT INTO tts_cache (..., audio_data) VALUES (..., ?BLOB)
               → PCM返却
```

**BLOB INSERT/SELECTの例:**

```rust
use rusqlite::params;

/// キャッシュ書き込み: Opusエンコード済みデータをBLOBとしてINSERT
fn cache_insert(
    conn: &rusqlite::Connection,
    cache_key: &str,
    text: &str,
    model: &str,
    speed: f64,
    opus_data: &[u8],
    duration_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO tts_cache
         (cache_key, text, model, speed, audio_format, audio_data, duration_ms)
         VALUES (?1, ?2, ?3, ?4, 'opus', ?5, ?6)",
        params![cache_key, text, model, speed, opus_data, duration_ms],
    )?;
    Ok(())
}

/// キャッシュ読み出し: BLOBからOpusデータを取得
fn cache_lookup(
    conn: &rusqlite::Connection,
    cache_key: &str,
) -> rusqlite::Result<Option<(Vec<u8>, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT audio_data, duration_ms FROM tts_cache WHERE cache_key = ?1"
    )?;
    let result = stmt.query_row(params![cache_key], |row| {
        let audio_data: Vec<u8> = row.get(0)?;
        let duration_ms: i64 = row.get(1)?;
        Ok((audio_data, duration_ms))
    });
    match result {
        Ok(data) => {
            // last_used_at と use_count を更新
            conn.execute(
                "UPDATE tts_cache SET last_used_at = datetime('now'),
                 use_count = use_count + 1 WHERE cache_key = ?1",
                params![cache_key],
            )?;
            Ok(Some(data))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}
```

#### 容量管理（Eviction）

- `max_entries` 超過時: `eviction_policy` に基づき古いエントリを削除
  - `lru`: `last_used_at` が最も古いものから削除
  - `ttl`: `created_at` から `ttl_days` 経過したものを削除
- `max_total_size_mb` 超過時: `SELECT SUM(length(audio_data))` で合計サイズを計算し、LRU順で閾値以下になるまで削除
- 削除はDELETE文のみ（ファイル削除不要）。定期的に `PRAGMA auto_vacuum = INCREMENTAL` + `PRAGMA incremental_vacuum` で空き領域を回収

## 17. マルチユーザーSTT（同時常時STT）

### 17.1 概要

ボイスチャンネルに複数ユーザーが参加している場合、最大**4人**まで同時にSTT（音声認識）を常時実行する。各ユーザーの音声はSSRC単位で分離され、それぞれ個別のSTT WebSocketセッションを持つ。

### 17.2 SSRC→UserIDマッピング

Discord Voice Gatewayでは、各ユーザーの音声パケットにSSRC（Synchronization Source Identifier）が付与される。SSRCとDiscord UserIDの紐付けは、**Voice State Update**イベント（ユーザーがVCに参加/移動した際に発火）で取得する。

```rust
use dashmap::DashMap;

/// SSRC → Discord UserID のマッピング管理
pub struct SsrcUserMap {
    /// SSRC → (user_id, username)
    ssrc_to_user: DashMap<u32, (u64, String)>,
    /// user_id → SSRC (逆引き)
    user_to_ssrc: DashMap<u64, u32>,
}

impl SsrcUserMap {
    pub fn new() -> Self {
        Self {
            ssrc_to_user: DashMap::new(),
            user_to_ssrc: DashMap::new(),
        }
    }

    /// Voice State UpdateからSSRCマッピングを更新
    /// songbirdのSpeakingUpdateイベントでSSRCとuser_idの紐付けを取得
    pub fn update_from_speaking(&self, ssrc: u32, user_id: u64, username: String) {
        self.ssrc_to_user.insert(ssrc, (user_id, username));
        self.user_to_ssrc.insert(user_id, ssrc);
    }

    /// SSRCからUserIDを取得
    pub fn get_user(&self, ssrc: u32) -> Option<(u64, String)> {
        self.ssrc_to_user.get(&ssrc).map(|r| r.value().clone())
    }

    /// ユーザー離脱時にマッピングを削除
    pub fn remove_user(&self, user_id: u64) {
        if let Some((_, ssrc)) = self.user_to_ssrc.remove(&user_id) {
            self.ssrc_to_user.remove(&ssrc);
        }
    }

    /// 現在追跡中のユーザー数
    pub fn active_count(&self) -> usize {
        self.ssrc_to_user.len()
    }
}
```

### 17.3 同時STTセッション数制限

最大4人まで同時にSTT WebSocketセッションを確立する。5人目以降はSTTセッションを接続せず、音声パケットを破棄する。

```rust
const MAX_CONCURRENT_STT: usize = 4;

impl Dispatcher {
    /// PCMチャンクを適切なWorkerセッションにルーティング（同時STT制限付き）
    async fn handle_audio(&self, chunk: AudioChunk) {
        let ssrc = chunk.ssrc;

        // 既存セッションがあれば転送
        if let Some(tx) = self.user_sessions.get(&ssrc) {
            let _ = tx.send(chunk.pcm).await;
            return;
        }

        // 同時STTセッション数チェック
        if self.user_sessions.len() >= MAX_CONCURRENT_STT {
            tracing::warn!(
                ssrc,
                active = self.user_sessions.len(),
                "Max concurrent STT sessions reached, ignoring audio"
            );
            return; // 5人目以降は接続しない
        }

        // 新規ユーザー: ストリーミングパイプラインを起動
        let (audio_tx, audio_rx) = mpsc::channel(256);
        let cancel = CancellationToken::new();

        self.user_sessions.insert(ssrc, audio_tx.clone());
        self.active_cancels.insert(ssrc, cancel.clone());

        let worker = Arc::clone(&self.worker);
        let playback_tx = self.playback_tx.clone();
        let sessions = self.user_sessions.clone();

        tokio::spawn(async move {
            if let Err(e) = worker.run_stream(
                ssrc as u64, audio_rx, playback_tx, cancel
            ).await {
                tracing::error!("Pipeline error for SSRC {}: {}", ssrc, e);
            }
            sessions.remove(&ssrc);
        });

        let _ = audio_tx.send(chunk.pcm).await;
    }
}
```

### 17.4 アーキテクチャ図

```
Discord VC (最大4人同時STT)
    │
    ├── SSRC=1001 (User A) ──► STT WebSocket #1 ──► SttEvent::Final
    ├── SSRC=1002 (User B) ──► STT WebSocket #2 ──► SttEvent::Final
    ├── SSRC=1003 (User C) ──► STT WebSocket #3 ──► SttEvent::Final
    ├── SSRC=1004 (User D) ──► STT WebSocket #4 ──► SttEvent::Final
    │
    └── SSRC=1005 (User E) ──✕ 接続しない（MAX_CONCURRENT_STT超過）
```

### 17.5 設定

```toml
# config.toml
[voice.stt]
max_concurrent_sessions = 4    # 同時STTセッション数上限（1-8、デフォルト: 4）
```

---

## 18. コンテキストウィンドウ方式mix（マルチユーザーバッチング）

### 18.1 概要

マルチユーザー環境では、複数ユーザーのSTT確定テキストを**2秒の時間窓（コンテキストウィンドウ）**内でバッチングし、発話者ラベル付きで1つのLLMリクエストにまとめる。これにより、同時発話や連続的な会話をLLMが文脈として理解し、自然なグループ対話が可能になる。

### 18.2 バッチングフロー

```
時刻 0.0s: User A の SttEvent::Final 「今日の天気は？」
時刻 0.5s: User B の SttEvent::Final 「それと明日も教えて」
時刻 1.2s: User A の SttEvent::Final 「東京で」
  │
  │  ← 2秒タイマー (最初のFinalから起算)
  │
時刻 2.0s: タイマー発火 → バッチをLLMに送信
  │
  ▼
LLMリクエスト:
  「Aさん: 今日の天気は？
   Bさん: それと明日も教えて
   Aさん: 東京で」
  │
  ▼
LLM応答 → TTS → VC再生
```

### 18.3 データ構造

```rust
use std::time::Instant;

/// 発話者ラベル付きテキスト
#[derive(Debug, Clone)]
pub struct LabeledUtterance {
    pub user_id: u64,
    pub username: String,
    pub text: String,
    pub timestamp: Instant,
}

/// コンテキストウィンドウバッファ
pub struct ContextWindowBuffer {
    utterances: Vec<LabeledUtterance>,
    window_start: Option<Instant>,
    window_duration: Duration,
}

impl ContextWindowBuffer {
    pub fn new(window_duration: Duration) -> Self {
        Self {
            utterances: Vec::new(),
            window_start: None,
            window_duration,
        }
    }

    /// STT確定テキストを追加
    pub fn push(&mut self, utterance: LabeledUtterance) {
        if self.window_start.is_none() {
            self.window_start = Some(Instant::now());
        }
        self.utterances.push(utterance);
    }

    /// 時間窓が満了したかチェック
    pub fn is_ready(&self) -> bool {
        self.window_start
            .map(|start| start.elapsed() >= self.window_duration)
            .unwrap_or(false)
    }

    /// バッファ内容を発話者ラベル付きテキストとして結合し、バッファをクリア
    pub fn flush(&mut self) -> Option<String> {
        if self.utterances.is_empty() {
            return None;
        }

        let text = self.utterances.iter()
            .map(|u| format!("{}さん: {}", u.username, u.text))
            .collect::<Vec<_>>()
            .join("\n");

        self.utterances.clear();
        self.window_start = None;
        Some(text)
    }
}
```

### 18.4 タイマーベースバッチング実装

```rust
use tokio::time::{sleep, Duration};

impl Dispatcher {
    /// マルチユーザーSTTイベントをバッチングしてLLMに送信
    async fn run_context_window(
        &self,
        mut stt_rx: mpsc::Receiver<LabeledUtterance>,
        ssrc_user_map: Arc<SsrcUserMap>,
    ) {
        let mut buffer = ContextWindowBuffer::new(
            Duration::from_millis(self.config.context_window_ms) // デフォルト: 2000ms
        );

        loop {
            tokio::select! {
                // STT確定テキスト受信
                Some(utterance) = stt_rx.recv() => {
                    buffer.push(utterance);
                }

                // タイマーチェック（100ms間隔でポーリング）
                _ = sleep(Duration::from_millis(100)) => {
                    if buffer.is_ready() {
                        if let Some(batch_text) = buffer.flush() {
                            tracing::info!("Context window flush:\n{}", batch_text);

                            // バッチテキストをLLMに送信
                            let playback_tx = self.playback_tx.clone();
                            let cancel = CancellationToken::new();
                            let worker = Arc::clone(&self.worker);

                            tokio::spawn(async move {
                                if let Err(e) = worker.process_text(
                                    0, // multi-user: user_id=0 (共有コンテキスト)
                                    &batch_text,
                                    &playback_tx,
                                    &cancel,
                                ).await {
                                    tracing::error!("Batch LLM error: {}", e);
                                }
                            });
                        }
                    }
                }
            }
        }
    }
}
```

### 18.5 LLMへの入力フォーマット

バッチングされたテキストは以下の形式でLLMに渡される。システムプロンプトにマルチユーザー対話であることを明示する:

```
[システムプロンプト追加]
現在、ボイスチャットで複数人と会話しています。
各発言は「名前さん: 内容」の形式で届きます。
文脈を考慮し、適切な相手に向けて応答してください。

[ユーザー入力]
Aさん: 今日の天気は？
Bさん: それと明日も教えて
Aさん: 東京で
```

### 18.6 シングルユーザーモードとの切り替え

VC内に1人しかいない場合はバッチングをバイパスし、従来の即時応答パイプライン（セクション7参照）を使用する。2人以上になった時点でコンテキストウィンドウ方式に自動切り替えする。

```rust
impl Dispatcher {
    fn should_use_batching(&self) -> bool {
        self.user_sessions.len() >= 2
    }
}
```

### 18.7 設定

```toml
# config.toml
[voice.pipeline]
# コンテキストウィンドウ（マルチユーザーバッチング）
context_window_ms = 2000           # 時間窓の長さ（ミリ秒）
context_window_auto = true         # 2人以上で自動有効化
```

---

## 19. モックSTTプロバイダ設計

### 19.1 概要

テスト用のモックSTTプロバイダ。`SttProvider` / `SttSession` traitを実装し、WebSocket接続をシミュレートする。実際のSTTサーバーを起動せずにパイプライン全体のテストを可能にする。

### 19.2 動作モデル

モックSTTは**シナリオ駆動**で動作する。事前定義された発話シナリオ（テキストリスト）を順番に `SttEvent` として返却する。

```rust
use std::time::Duration;

/// モックSTTの発話シナリオ
#[derive(Debug, Clone)]
pub struct MockUtterance {
    /// 確定テキスト
    pub text: String,
    /// 言語コード
    pub language: String,
    /// speech_start送信までの遅延（レイテンシシミュレーション）
    pub delay_before_start: Duration,
    /// partial送信間隔
    pub partial_interval: Duration,
    /// final送信までの遅延（speech_start起算）
    pub delay_to_final: Duration,
    /// 信頼度スコア
    pub confidence: f32,
}

/// モックSTTプロバイダ設定
#[derive(Debug, Clone)]
pub struct MockSttConfig {
    /// 返却する発話シナリオのリスト（順番に消費される）
    pub utterances: Vec<MockUtterance>,
    /// 全シナリオ消費後にセッションを閉じるか（false=最後の発話後も待機）
    pub close_after_all: bool,
    /// グローバル遅延乗数（1.0=設定通り、2.0=2倍遅延）
    pub latency_multiplier: f64,
}
```

### 19.3 イベント送出シーケンス

各 `MockUtterance` に対して以下の順序でイベントを送出する:

```
[delay_before_start 待機]
  ↓
SttEvent::SpeechStart { timestamp_ms }
  ↓
[partial_interval 間隔で partial を複数回送出]
SttEvent::Partial { text: "こん" }
SttEvent::Partial { text: "こんにち" }
SttEvent::Partial { text: "こんにちは" }
  ↓
[delay_to_final 到達]
SttEvent::Final { text: "こんにちは", language, confidence, duration_ms }
  ↓
SttEvent::SpeechEnd { timestamp_ms, duration_ms }
```

partial テキストは `text` を先頭から段階的に切り出して生成する（文字数を `partial_interval` 回数で等分）。

### 19.4 Rust実装スケッチ

```rust
pub struct MockSttProvider {
    config: MockSttConfig,
}

#[async_trait]
impl SttProvider for MockSttProvider {
    async fn connect(&self) -> Result<Box<dyn SttSession>> {
        Ok(Box::new(MockSttSession::new(self.config.clone())))
    }

    fn name(&self) -> &str { "mock" }
}

pub struct MockSttSession {
    config: MockSttConfig,
    utterance_index: usize,
    /// 現在の発話内のイベント進行状態
    event_phase: EventPhase,
    /// send_audioで受信したバイト数（発話トリガーに使用）
    audio_bytes_received: usize,
    /// 割り込みシミュレーション用: 外部から任意タイミングでspeech_startを注入
    interrupt_trigger: Option<tokio::sync::mpsc::Receiver<()>>,
}

#[derive(Debug)]
enum EventPhase {
    WaitingForAudio,
    DelayBeforeStart,
    SpeechStarted { partial_count: usize, partial_total: usize },
    FinalSent,
    SpeechEndSent,
}

#[async_trait]
impl SttSession for MockSttSession {
    async fn send_audio(&mut self, _audio: &[f32]) -> Result<()> {
        // 音声データ受信をカウント（一定量受信後にシナリオを進行）
        self.audio_bytes_received += _audio.len();
        Ok(())
    }

    async fn recv_event(&mut self) -> Result<Option<SttEvent>> {
        // utterance_indexに基づいて現在のシナリオからイベントを生成
        // EventPhaseの状態機械に従って適切なSttEventを返却
        // 全シナリオ消費後はNone（close_after_all=true）または永久待機
        todo!("状態機械に基づくイベント送出")
    }

    async fn close(&mut self) -> Result<()> { Ok(()) }
}
```

### 19.5 割り込みシミュレーション

テスト用に外部から任意タイミングで `SpeechStart` を注入できるチャンネルを提供する。barge-inテストで使用する。

```rust
impl MockSttSession {
    /// 割り込みトリガーチャンネルを設定
    /// テストコードからtx.send(())すると、次のrecv_eventでSpeechStartを返す
    pub fn set_interrupt_trigger(&mut self, rx: mpsc::Receiver<()>) {
        self.interrupt_trigger = Some(rx);
    }
}

// テストコード側:
let (interrupt_tx, interrupt_rx) = mpsc::channel(1);
session.set_interrupt_trigger(interrupt_rx);

// Bot再生中に割り込みを発火
interrupt_tx.send(()).await.unwrap();
```

### 19.6 設定ファイルによるシナリオ定義

```toml
# test_scenarios/basic_conversation.toml
[[utterances]]
text = "こんにちは"
language = "ja"
delay_before_start_ms = 100
partial_interval_ms = 50
delay_to_final_ms = 300
confidence = 0.95

[[utterances]]
text = "今日の天気を教えて"
language = "ja"
delay_before_start_ms = 500
partial_interval_ms = 80
delay_to_final_ms = 600
confidence = 0.92

[[utterances]]
text = "ありがとう"
language = "ja"
delay_before_start_ms = 200
partial_interval_ms = 40
delay_to_final_ms = 250
confidence = 0.98
```

---

## 20. モックTTSプロバイダ設計

### 20.1 概要

テスト用のモックTTSプロバイダ。`TtsProvider` traitを実装し、テキストを受け取って無音PCMデータ（またはサイン波）を返す。実際のTTSサーバーなしでパイプラインテストを可能にする。

### 20.2 設定

```rust
/// モックTTSプロバイダ設定
#[derive(Debug, Clone)]
pub struct MockTtsConfig {
    /// 出力サンプルレート
    pub sample_rate: u32,           // デフォルト: 24000
    /// 1文字あたりの音声時間（ミリ秒）
    pub ms_per_char: f64,           // デフォルト: 150.0
    /// 最小duration（短すぎるテキスト対策）
    pub min_duration_ms: f64,       // デフォルト: 200.0
    /// 最大duration（長すぎるテキスト対策）
    pub max_duration_ms: f64,       // デフォルト: 30000.0
    /// 出力波形タイプ
    pub waveform: MockWaveform,     // デフォルト: Silence
    /// ランダム遅延範囲（キャッシュテスト用）
    pub random_delay: Option<RandomDelay>,
}

#[derive(Debug, Clone)]
pub enum MockWaveform {
    /// 無音（全サンプル0.0）
    Silence,
    /// サイン波（周波数指定、波形確認テスト用）
    Sine { frequency_hz: f32, amplitude: f32 },
}

#[derive(Debug, Clone)]
pub struct RandomDelay {
    /// 最小遅延（ミリ秒）
    pub min_ms: u64,
    /// 最大遅延（ミリ秒）
    pub max_ms: u64,
}
```

### 20.3 duration_ms自動計算

テキスト長から音声の長さを自動計算する:

```
duration_ms = clamp(
    text.chars().count() as f64 * ms_per_char,
    min_duration_ms,
    max_duration_ms,
)
```

例: `ms_per_char = 150.0` の場合:
- "こんにちは"（5文字）→ 750ms
- "今日の天気は晴れです"（9文字）→ 1350ms
- "あ"（1文字）→ 200ms（min_duration_ms適用）

### 20.4 Rust実装

```rust
use rand::Rng;

pub struct MockTtsProvider {
    config: MockTtsConfig,
}

#[async_trait]
impl TtsProvider for MockTtsProvider {
    async fn synthesize(&self, text: &str) -> Result<TtsResult> {
        // ランダム遅延（キャッシュテスト: 遅延ありでミスを検知しやすくする）
        if let Some(ref delay) = self.config.random_delay {
            let ms = rand::thread_rng().gen_range(delay.min_ms..=delay.max_ms);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }

        // duration計算
        let char_count = text.chars().count() as f64;
        let duration_ms = (char_count * self.config.ms_per_char)
            .clamp(self.config.min_duration_ms, self.config.max_duration_ms);

        let num_samples = (self.config.sample_rate as f64 * duration_ms / 1000.0) as usize;

        // 波形生成
        let audio = match &self.config.waveform {
            MockWaveform::Silence => vec![0.0f32; num_samples],
            MockWaveform::Sine { frequency_hz, amplitude } => {
                (0..num_samples)
                    .map(|i| {
                        let t = i as f32 / self.config.sample_rate as f32;
                        amplitude * (2.0 * std::f32::consts::PI * frequency_hz * t).sin()
                    })
                    .collect()
            }
        };

        Ok(TtsResult {
            audio,
            sample_rate: self.config.sample_rate,
            duration_ms,
        })
    }

    fn name(&self) -> &str { "mock" }
}
```

### 20.5 テスト用ファクトリ

```rust
impl MockTtsProvider {
    /// デフォルト設定（無音、遅延なし）でモックTTSを生成
    pub fn silent() -> Self {
        Self {
            config: MockTtsConfig {
                sample_rate: 24000,
                ms_per_char: 150.0,
                min_duration_ms: 200.0,
                max_duration_ms: 30000.0,
                waveform: MockWaveform::Silence,
                random_delay: None,
            },
        }
    }

    /// ランダム遅延付き（キャッシュテスト用）
    pub fn with_random_delay(min_ms: u64, max_ms: u64) -> Self {
        Self {
            config: MockTtsConfig {
                random_delay: Some(RandomDelay { min_ms, max_ms }),
                ..Self::silent().config
            },
        }
    }

    /// サイン波出力（波形検証テスト用）
    pub fn sine(frequency_hz: f32) -> Self {
        Self {
            config: MockTtsConfig {
                waveform: MockWaveform::Sine {
                    frequency_hz,
                    amplitude: 0.5,
                },
                ..Self::silent().config
            },
        }
    }
}
```

### 20.8 テキストチャンネル連動（VC連動テキスト投稿との統合）

モックTTSは音声生成の代わりにテキストをログ出力する。さらに、§26で定義するVC連動テキスト投稿機能が有効な場合、モックTTSもテキストチャンネルへの投稿を行う。

```
[モックTTSの動作]
1. テキストを受け取る
2. ログ出力: info!("MockTTS: generating {} ms audio for: {}", duration_ms, text)
3. VC連動テキスト投稿が有効なら → テキストチャンネルに「🔊 Bot名: 応答テキスト」を投稿
4. 無音/サイン波PCMデータを返す
```

これにより、実際のTTSサーバーなしでもパイプライン全体の流れをテキストベースで確認可能となる。STT→LLM→TTS→テキスト投稿の一連のフローが、すべてログとテキストチャンネルで可視化される。

```rust
// MockTtsProvider::synthesize 内（擬似コード）
async fn synthesize(&self, text: &str) -> Result<AudioData> {
    let duration_ms = self.calculate_duration(text);
    info!("MockTTS: generating {} ms audio for: {:?}", duration_ms, text);

    // VC連動テキスト投稿（§26）が有効なら投稿
    if let Some(ref transcript_tx) = self.transcript_sender {
        transcript_tx.send(TranscriptEntry::BotResponse {
            bot_name: self.bot_name.clone(),
            text: text.to_string(),
        }).await?;
    }

    Ok(self.generate_waveform(duration_ms))
}
```

---

## 21. テストコード設計

### 21.1 テスト方針

すべてのテストは `cargo test --features voice` で実行可能であること。外部サービス（Discord、STTサーバー、TTSサーバー）への依存は一切なく、モックプロバイダのみで完結する。

### 21.2 ディレクトリ構成

```
localgpt/
├── src/voice/
│   ├── provider/
│   │   ├── stt/
│   │   │   ├── mock.rs          # MockSttProvider / MockSttSession
│   │   │   └── ws.rs
│   │   └── tts/
│   │       ├── mock.rs          # MockTtsProvider
│   │       └── aivis_speech.rs
│   └── ...
│
└── tests/                        # 結合テスト
    └── voice/
        ├── mod.rs
        ├── helpers.rs            # テストヘルパー
        ├── test_pipeline.rs      # パイプラインE2Eテスト
        ├── test_barge_in.rs      # 割り込みテスト
        ├── test_segment_order.rs # セグメント順序保証テスト
        ├── test_cache.rs         # TTSキャッシュテスト
        └── test_multi_user.rs    # マルチユーザーバッチングテスト
```

### 21.3 ユニットテスト

各モジュール内の `#[cfg(test)] mod tests` で実装する。

#### 21.3.1 Provider trait実装テスト

```rust
// src/voice/provider/stt/mock.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_stt_basic_utterance() {
        let config = MockSttConfig {
            utterances: vec![MockUtterance {
                text: "こんにちは".to_string(),
                language: "ja".to_string(),
                delay_before_start: Duration::from_millis(10),
                partial_interval: Duration::from_millis(5),
                delay_to_final: Duration::from_millis(30),
                confidence: 0.95,
            }],
            close_after_all: true,
            latency_multiplier: 1.0,
        };

        let provider = MockSttProvider { config };
        let mut session = provider.connect().await.unwrap();

        // 音声データを送信（トリガー）
        session.send_audio(&vec![0.0; 320]).await.unwrap();

        // イベント受信: speech_start → partial* → final → speech_end
        let mut events = Vec::new();
        while let Some(event) = session.recv_event().await.unwrap() {
            events.push(event);
        }

        // 検証: SpeechStart, Partial+, Final, SpeechEnd の順序
        assert!(matches!(events.first(), Some(SttEvent::SpeechStart { .. })));
        assert!(matches!(events.last(), Some(SttEvent::SpeechEnd { .. })));

        let final_event = events.iter().find(|e| matches!(e, SttEvent::Final { .. }));
        assert!(final_event.is_some());
        if let Some(SttEvent::Final { text, .. }) = final_event {
            assert_eq!(text, "こんにちは");
        }
    }

    #[tokio::test]
    async fn test_mock_stt_multiple_utterances() {
        // 複数発話シナリオが順番に消費されることを確認
    }

    #[tokio::test]
    async fn test_mock_stt_latency_multiplier() {
        // latency_multiplier=2.0で遅延が2倍になることを確認
    }
}
```

```rust
// src/voice/provider/tts/mock.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_tts_duration_calculation() {
        let tts = MockTtsProvider::silent();
        let result = tts.synthesize("こんにちは").await.unwrap(); // 5文字
        // 5 * 150ms = 750ms
        assert!((result.duration_ms - 750.0).abs() < 1.0);
        assert_eq!(result.sample_rate, 24000);
        let expected_samples = (24000.0 * 0.75) as usize;
        assert_eq!(result.audio.len(), expected_samples);
    }

    #[tokio::test]
    async fn test_mock_tts_min_duration() {
        let tts = MockTtsProvider::silent();
        let result = tts.synthesize("あ").await.unwrap(); // 1文字 = 150ms < min 200ms
        assert!((result.duration_ms - 200.0).abs() < 1.0);
    }

    #[tokio::test]
    async fn test_mock_tts_sine_wave() {
        let tts = MockTtsProvider::sine(440.0);
        let result = tts.synthesize("テスト").await.unwrap();
        // サイン波であること（振幅が0でない）
        let max_amplitude = result.audio.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(max_amplitude > 0.4); // amplitude=0.5
    }

    #[tokio::test]
    async fn test_mock_tts_random_delay() {
        let tts = MockTtsProvider::with_random_delay(50, 100);
        let start = Instant::now();
        let _ = tts.synthesize("テスト").await.unwrap();
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(50));
    }
}
```

#### 21.3.2 文分割ロジックテスト

```rust
// src/voice/worker.rs または split_into_sentences のあるモジュール
#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// ヘルパー: トークン列からストリームを生成
    fn tokens_to_stream(tokens: Vec<&str>) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
        let tokens: Vec<String> = tokens.into_iter().map(|s| s.to_string()).collect();
        Box::pin(futures::stream::iter(tokens.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn test_split_basic_sentences() {
        let stream = tokens_to_stream(vec!["こんに", "ちは。", "元気", "ですか？"]);
        let sentences: Vec<String> = split_into_sentences(stream)
            .collect::<Vec<Result<String>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(sentences, vec!["こんにちは。", "元気ですか？"]);
    }

    #[tokio::test]
    async fn test_split_paragraph_break() {
        let stream = tokens_to_stream(vec!["段落1", "\n\n", "段落2"]);
        let sentences: Vec<String> = split_into_sentences(stream)
            .collect::<Vec<Result<String>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(sentences, vec!["段落1", "段落2"]);
    }

    #[tokio::test]
    async fn test_split_flush_remaining() {
        // 句読点なしでストリーム終了 → 残りをフラッシュ
        let stream = tokens_to_stream(vec!["最後まで", "句読点なし"]);
        let sentences: Vec<String> = split_into_sentences(stream)
            .collect::<Vec<Result<String>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(sentences, vec!["最後まで句読点なし"]);
    }

    #[tokio::test]
    async fn test_split_mixed_delimiters() {
        let stream = tokens_to_stream(vec!["すごい！", "本当？", "はい。"]);
        let sentences: Vec<String> = split_into_sentences(stream)
            .collect::<Vec<Result<String>>>()
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(sentences, vec!["すごい！", "本当？", "はい。"]);
    }
}
```

#### 21.3.3 TTSキャッシュDB操作テスト

```rust
// src/voice/cache.rs
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // スキーマ作成（セクション16.4参照）
        conn.execute_batch(include_str!("../../sql/tts_cache_schema.sql")).unwrap();
        conn
    }

    #[test]
    fn test_cache_insert_and_lookup() {
        let conn = setup_test_db();
        let key = generate_cache_key("こんにちは", "mock", 1.0, None, None, None);
        let opus_data = vec![0u8; 100]; // ダミーOpusデータ

        cache_insert(&conn, &key, "こんにちは", "mock", 1.0, &opus_data, 750).unwrap();

        let result = cache_lookup(&conn, &key).unwrap();
        assert!(result.is_some());
        let (data, duration) = result.unwrap();
        assert_eq!(data.len(), 100);
        assert_eq!(duration, 750);
    }

    #[test]
    fn test_cache_miss() {
        let conn = setup_test_db();
        let key = generate_cache_key("存在しない", "mock", 1.0, None, None, None);
        let result = cache_lookup(&conn, &key).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_cache_use_count_increment() {
        let conn = setup_test_db();
        let key = generate_cache_key("テスト", "mock", 1.0, None, None, None);
        cache_insert(&conn, &key, "テスト", "mock", 1.0, &[0u8; 50], 500).unwrap();

        // 3回lookup
        for _ in 0..3 {
            cache_lookup(&conn, &key).unwrap();
        }

        // use_countが4（insert時1 + lookup3回）であることを確認
        let count: i64 = conn.query_row(
            "SELECT use_count FROM tts_cache WHERE cache_key = ?1",
            [&key], |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn test_cache_eviction_lru() {
        // max_entries=3で4つ挿入 → 最もlast_used_atが古いものが削除される
    }

    #[test]
    fn test_cache_key_different_params() {
        // 同じテキストでもspeed/style_idが異なればキーが違う
        let key1 = generate_cache_key("テスト", "mock", 1.0, Some(1), None, None);
        let key2 = generate_cache_key("テスト", "mock", 1.5, Some(1), None, None);
        assert_ne!(key1, key2);
    }
}
```

#### 21.3.4 コンテキストウィンドウバッファテスト

```rust
// src/voice/dispatcher.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_window_push_and_flush() {
        let mut buffer = ContextWindowBuffer::new(Duration::from_millis(2000));

        buffer.push(LabeledUtterance {
            user_id: 1,
            username: "Alice".to_string(),
            text: "こんにちは".to_string(),
            timestamp: Instant::now(),
        });
        buffer.push(LabeledUtterance {
            user_id: 2,
            username: "Bob".to_string(),
            text: "やあ".to_string(),
            timestamp: Instant::now(),
        });

        let result = buffer.flush().unwrap();
        assert!(result.contains("Aliceさん: こんにちは"));
        assert!(result.contains("Bobさん: やあ"));
    }

    #[test]
    fn test_context_window_empty_flush() {
        let mut buffer = ContextWindowBuffer::new(Duration::from_millis(2000));
        assert!(buffer.flush().is_none());
    }

    #[tokio::test]
    async fn test_context_window_is_ready() {
        let mut buffer = ContextWindowBuffer::new(Duration::from_millis(50));
        buffer.push(LabeledUtterance {
            user_id: 1,
            username: "Test".to_string(),
            text: "テスト".to_string(),
            timestamp: Instant::now(),
        });
        assert!(!buffer.is_ready());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(buffer.is_ready());
    }
}
```

### 21.4 結合テスト

`tests/voice/` ディレクトリに配置する結合テスト。モックSTT/TTSを使い、パイプライン全体のE2Eフローを検証する。

#### 21.4.1 テストヘルパー

```rust
// tests/voice/helpers.rs

use localgpt::voice::provider::stt::mock::*;
use localgpt::voice::provider::tts::mock::*;
use localgpt::voice::*;
use std::time::Duration;

/// テスト用のパイプライン設定を生成
pub fn test_config() -> VoiceConfig {
    VoiceConfig {
        enabled: true,
        pipeline: PipelineConfig {
            interrupt_enabled: true,
            context_window_ms: 2000,
            context_window_auto: true,
        },
        stt: SttConfig {
            provider: "mock".to_string(),
            max_concurrent_sessions: 4,
            ..Default::default()
        },
        tts: TtsConfig {
            provider: "mock".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// 基本的なモックSTTプロバイダを生成（単一発話）
pub fn mock_stt_single(text: &str) -> MockSttProvider {
    MockSttProvider {
        config: MockSttConfig {
            utterances: vec![MockUtterance {
                text: text.to_string(),
                language: "ja".to_string(),
                delay_before_start: Duration::from_millis(10),
                partial_interval: Duration::from_millis(5),
                delay_to_final: Duration::from_millis(30),
                confidence: 0.95,
            }],
            close_after_all: true,
            latency_multiplier: 1.0,
        },
    }
}

/// 複数発話シナリオのモックSTTプロバイダを生成
pub fn mock_stt_multi(texts: Vec<&str>) -> MockSttProvider {
    let utterances = texts.into_iter().map(|text| MockUtterance {
        text: text.to_string(),
        language: "ja".to_string(),
        delay_before_start: Duration::from_millis(10),
        partial_interval: Duration::from_millis(5),
        delay_to_final: Duration::from_millis(30),
        confidence: 0.95,
    }).collect();

    MockSttProvider {
        config: MockSttConfig {
            utterances,
            close_after_all: true,
            latency_multiplier: 1.0,
        },
    }
}

/// テスト用のモックAgentBridge（固定応答）
pub struct MockAgentBridge {
    pub responses: std::collections::HashMap<String, String>,
}

impl MockAgentBridge {
    pub fn with_response(input: &str, output: &str) -> Self {
        let mut responses = std::collections::HashMap::new();
        responses.insert(input.to_string(), output.to_string());
        Self { responses }
    }

    pub fn echo() -> Self {
        Self { responses: std::collections::HashMap::new() }
        // responsesが空の場合は入力をそのままエコーバック
    }
}
```

#### 21.4.2 シングルユーザー基本フローテスト

```rust
// tests/voice/test_pipeline.rs

#[tokio::test]
async fn test_single_user_basic_flow() {
    // Setup: モックSTT（"こんにちは"を返す）+ モックAgent（固定応答）+ モックTTS
    let stt = Arc::new(mock_stt_single("こんにちは"));
    let agent = Arc::new(MockAgentBridge::with_response(
        "こんにちは", "こんにちは！今日はいい天気ですね。"
    ));
    let tts = Arc::new(MockTtsProvider::silent());
    let (playback_tx, mut playback_rx) = mpsc::channel(32);

    let worker = PipelineWorker { stt, agent_bridge: agent, tts };
    let (audio_tx, audio_rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();

    // ワーカー起動
    let handle = tokio::spawn(async move {
        worker.run_stream(1, audio_rx, playback_tx, cancel).await
    });

    // ダミー音声送信
    audio_tx.send(vec![0.0; 320]).await.unwrap();

    // 再生コマンド受信を待つ
    let cmd = tokio::time::timeout(Duration::from_secs(5), playback_rx.recv())
        .await.unwrap().unwrap();

    assert!(matches!(cmd, PlaybackCommand::Play(_)));
}
```

#### 21.4.3 マルチユーザーバッチングテスト

```rust
// tests/voice/test_multi_user.rs

#[tokio::test]
async fn test_multi_user_batching() {
    // 2ユーザーのSTT確定テキストがコンテキストウィンドウ内でバッチングされる
    let (stt_tx, stt_rx) = mpsc::channel(32);

    // User A の発話
    stt_tx.send(LabeledUtterance {
        user_id: 1,
        username: "Alice".to_string(),
        text: "今日の天気は？".to_string(),
        timestamp: Instant::now(),
    }).await.unwrap();

    // User B の発話（500ms後）
    tokio::time::sleep(Duration::from_millis(500)).await;
    stt_tx.send(LabeledUtterance {
        user_id: 2,
        username: "Bob".to_string(),
        text: "東京で教えて".to_string(),
        timestamp: Instant::now(),
    }).await.unwrap();

    // コンテキストウィンドウ満了を待つ
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // LLMに渡されたテキストが両方の発話を含むことを検証
    // （Dispatcher内部のflushを検証）
}
```

#### 21.4.4 割り込み（Barge-in）テスト

```rust
// tests/voice/test_barge_in.rs

#[tokio::test]
async fn test_barge_in_stops_playback() {
    // Setup: 長い応答を返すAgent + モックTTS
    // 1. STTが"長い話をして"を返す
    // 2. Agentが3文の応答を生成
    // 3. seg0再生完了後、seg1再生中に割り込みspeech_startを注入
    // 4. 再生が停止し、seg2以降がキャンセルされることを確認
    // 5. 会話履歴にseg0のテキストのみが記録されていることを確認

    let (interrupt_tx, interrupt_rx) = mpsc::channel(1);

    let stt = Arc::new(MockSttProvider {
        config: MockSttConfig {
            utterances: vec![
                MockUtterance {
                    text: "長い話をして".to_string(),
                    language: "ja".to_string(),
                    delay_before_start: Duration::from_millis(10),
                    partial_interval: Duration::from_millis(5),
                    delay_to_final: Duration::from_millis(30),
                    confidence: 0.95,
                },
            ],
            close_after_all: false, // セッション維持（割り込み用）
            latency_multiplier: 1.0,
        },
    });

    // Agent: 3文の応答
    let agent = Arc::new(MockAgentBridge::with_response(
        "長い話をして",
        "最初の文です。次の文です。最後の文です。"
    ));

    // TTS: 各文500msの遅延（再生時間シミュレーション）
    let tts = Arc::new(MockTtsProvider::with_random_delay(500, 500));

    let (playback_tx, mut playback_rx) = mpsc::channel(32);

    // ... ワーカー起動・割り込み注入・検証 ...

    // seg0再生完了を待つ
    let _seg0 = playback_rx.recv().await.unwrap();

    // 割り込み発火
    interrupt_tx.send(()).await.unwrap();

    // 以降のPlaybackCommandがStop or キャンセルであることを確認
}

#[tokio::test]
async fn test_barge_in_history_correctness() {
    // 割り込み後の会話履歴に、再生完了済みセグメントのみが記録される
}
```

#### 21.4.5 セグメント順序保証テスト

```rust
// tests/voice/test_segment_order.rs

#[tokio::test]
async fn test_segment_order_guaranteed() {
    // seg2がseg1より先に完了しても、再生順は seg0 → seg1 → seg2

    // MockTTS: seg1に長い遅延、seg2は即座に完了
    // seg0: 50ms, seg1: 500ms, seg2: 50ms
    // 期待: 再生順は seg0 → seg1 → seg2

    let (playback_tx, mut playback_rx) = mpsc::channel(32);

    // PlaybackOrchestratorの順序保証を直接テスト
    let mut orchestrator = PlaybackOrchestrator::new();

    // seg2を先に完了通知
    orchestrator.on_segment_ready(TTSSegment {
        seg_index: 2,
        text: "最後。".to_string(),
        request_id: "test".to_string(),
        audio_path: None,
        status: "ready".to_string(),
    }).await;

    // seg0を完了通知
    orchestrator.on_segment_ready(TTSSegment {
        seg_index: 0,
        text: "最初。".to_string(),
        request_id: "test".to_string(),
        audio_path: None,
        status: "ready".to_string(),
    }).await;

    // この時点で再生されるのはseg0のみ（seg1がまだ）
    assert_eq!(orchestrator.next_play_index, 0);

    // seg1を完了通知
    orchestrator.on_segment_ready(TTSSegment {
        seg_index: 1,
        text: "中間。".to_string(),
        request_id: "test".to_string(),
        audio_path: None,
        status: "ready".to_string(),
    }).await;

    // seg0 → seg1 → seg2 の順で再生されることを確認
}
```

#### 21.4.6 TTSキャッシュヒット/ミステスト

```rust
// tests/voice/test_cache.rs

#[tokio::test]
async fn test_tts_cache_hit() {
    let conn = Connection::open_in_memory().unwrap();
    setup_cache_schema(&conn);

    let tts = MockTtsProvider::with_random_delay(100, 200);

    // 1回目: キャッシュミス → TTS合成実行（100-200ms遅延）
    let start = Instant::now();
    let result1 = cached_synthesize(&conn, &tts, "テスト", "mock", 1.0).await.unwrap();
    let first_duration = start.elapsed();

    // 2回目: キャッシュヒット → 遅延なし
    let start = Instant::now();
    let result2 = cached_synthesize(&conn, &tts, "テスト", "mock", 1.0).await.unwrap();
    let second_duration = start.elapsed();

    // キャッシュヒット時は大幅に速い
    assert!(second_duration < first_duration / 2);

    // 音声データが同一であること
    assert_eq!(result1.duration_ms, result2.duration_ms);
}

#[tokio::test]
async fn test_tts_cache_different_params_miss() {
    // 同じテキストでもパラメータが異なればミス
    let conn = Connection::open_in_memory().unwrap();
    setup_cache_schema(&conn);

    let tts = MockTtsProvider::silent();

    cached_synthesize(&conn, &tts, "テスト", "mock", 1.0).await.unwrap();

    // speed=1.5でキャッシュミスになることを確認
    // （use_countが増えないことで検証）
}
```

### 21.5 テスト実行

```bash
# ユニットテスト + 結合テスト（voice feature有効）
cargo test --features voice

# 特定のテストのみ実行
cargo test --features voice test_barge_in

# テスト詳細出力
cargo test --features voice -- --nocapture
```

---

## 22. レビューフィードバック反映: VC接続状態機械

### 22.1 概要

しのえもんレビューのフィードバックに基づき、VC接続管理を明示的な状態機械（State Machine）として設計する。セクション6のVoiceManager/songbird連携を補完し、接続ライフサイクルの堅牢性を向上させる。

### 22.2 状態遷移図

```
                  join()
    ┌──────────┐ ──────► ┌────────────┐
    │Disconnected│        │ Connecting │
    └──────────┘ ◄────── └─────┬──────┘
         ▲        timeout/     │ Voice Server Update
         │        error        │ + ConnectionInfo構築成功
         │                     ▼
         │              ┌───────────┐
         │  leave()     │ Connected │
         │ ◄────────────┤           │
         │              └─────┬─────┘
         │                    │ connection lost
         │                    │ (songbird error event)
         │                    ▼
         │            ┌──────────────┐
         │            │ Reconnecting │
         │ ◄──────────┤              │
         │  max_retries└──────┬──────┘
         │  exceeded          │ reconnect success
                              ▼
                        ┌───────────┐
                        │ Connected │
                        └───────────┘
```

### 22.3 状態定義

```rust
use std::time::Instant;

#[derive(Debug, Clone, PartialEq)]
pub enum VcConnectionState {
    /// 未接続
    Disconnected,

    /// 接続中（Voice State Update送信済み、Voice Server Update待ち）
    Connecting {
        started_at: Instant,
        guild_id: u64,
        channel_id: u64,
    },

    /// 接続完了（音声送受信可能）
    Connected {
        guild_id: u64,
        channel_id: u64,
        connected_at: Instant,
    },

    /// 再接続中（接続断検知後の自動復旧）
    Reconnecting {
        guild_id: u64,
        channel_id: u64,
        attempt: u32,
        max_attempts: u32,
        last_attempt_at: Instant,
    },
}

impl VcConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Connected { .. } | Self::Reconnecting { .. })
    }
}
```

### 22.4 状態遷移ロジック

```rust
impl VoiceManager {
    /// 状態遷移を実行（不正な遷移はエラー）
    fn transition(&mut self, guild_id: u64, new_state: VcConnectionState) -> Result<()> {
        let current = self.connection_states.get(&guild_id)
            .map(|r| r.clone())
            .unwrap_or(VcConnectionState::Disconnected);

        // 許可される遷移のみ実行
        let valid = match (&current, &new_state) {
            (VcConnectionState::Disconnected, VcConnectionState::Connecting { .. }) => true,
            (VcConnectionState::Connecting { .. }, VcConnectionState::Connected { .. }) => true,
            (VcConnectionState::Connecting { .. }, VcConnectionState::Disconnected) => true, // timeout
            (VcConnectionState::Connected { .. }, VcConnectionState::Disconnected) => true,  // leave
            (VcConnectionState::Connected { .. }, VcConnectionState::Reconnecting { .. }) => true,
            (VcConnectionState::Reconnecting { .. }, VcConnectionState::Connected { .. }) => true,
            (VcConnectionState::Reconnecting { .. }, VcConnectionState::Disconnected) => true, // give up
            _ => false,
        };

        if !valid {
            anyhow::bail!(
                "Invalid VC state transition: {:?} → {:?}",
                current, new_state
            );
        }

        tracing::info!(
            guild_id,
            from = ?current,
            to = ?new_state,
            "VC state transition"
        );

        self.connection_states.insert(guild_id, new_state);
        Ok(())
    }
}
```

### 22.5 接続タイムアウトと再接続設定

```toml
# config.toml
[voice.connection]
connect_timeout_ms = 10000         # Connecting → timeout → Disconnected
reconnect_interval_ms = 1000       # 再接続試行間隔
reconnect_backoff_multiplier = 2.0 # 指数バックオフ乗数
max_reconnect_attempts = 5         # 最大再接続回数
```

---

## 23. レビューフィードバック反映: SttEvent拡張

### 23.1 SttEvent に Cancel / Reset を追加

しのえもんレビューのフィードバックに基づき、`SttEvent` に `Cancel` と `Reset` バリアントを追加する。割り込み処理やセッションリセットを明示的にイベントとして表現する。

```rust
/// STTサーバーから受信するイベント（拡張版）
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SttEvent {
    /// 発話開始検出（サーバー側VAD）
    #[serde(rename = "speech_start")]
    SpeechStart { timestamp_ms: u64 },

    /// 中間認識結果
    #[serde(rename = "partial")]
    Partial { text: String },

    /// 確定認識結果
    #[serde(rename = "final")]
    Final {
        text: String,
        language: String,
        confidence: f32,
        duration_ms: f64,
    },

    /// 発話終了検出
    #[serde(rename = "speech_end")]
    SpeechEnd { timestamp_ms: u64, duration_ms: f64 },

    /// 認識キャンセル（割り込み等で現在の認識を破棄）
    /// STTサーバー側で発話が短すぎる・ノイズと判定した場合にも送信される
    #[serde(rename = "cancel")]
    Cancel {
        /// キャンセル理由
        reason: CancelReason,
    },

    /// セッションリセット（内部状態を初期化、新しい発話の受付を再開）
    /// クライアントからの明示的リセット要求への応答、またはサーバー側の自動リセット
    #[serde(rename = "reset")]
    Reset {
        /// リセット理由
        reason: ResetReason,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// 割り込み（barge-in）による中断
    Interrupt,
    /// 発話が短すぎる（ノイズ等）
    TooShort,
    /// クライアントからの明示的キャンセル
    ClientRequest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    /// 割り込み後のリセット
    PostInterrupt,
    /// タイムアウト後のリセット
    Timeout,
    /// クライアントからの明示的リセット
    ClientRequest,
}
```

### 23.2 PipelineWorkerでのCancel/Reset処理

```rust
// PipelineWorker::run_stream 内のイベントハンドリング拡張
Some(SttEvent::Cancel { reason }) => {
    tracing::info!(user_id, ?reason, "STT cancel");
    // 現在進行中のAgent/TTSパイプラインをキャンセル
    match reason {
        CancelReason::Interrupt => {
            // barge-in: 再生停止 + パイプラインキャンセル
            let _ = playback_tx.send(PlaybackCommand::Stop).await;
        }
        CancelReason::TooShort | CancelReason::ClientRequest => {
            // 無視（パイプライン未起動のため何もしない）
        }
    }
}
Some(SttEvent::Reset { reason }) => {
    tracing::info!(user_id, ?reason, "STT reset");
    // セッション内部状態のリセット確認
    // 次の発話を受け付け可能な状態に戻る
}
```

### 23.3 クライアント→サーバーのCancel/Resetコマンド

STT WebSocketでクライアントからサーバーへ送信するテキストフレーム:

```json
// 現在の認識をキャンセル（割り込み検知時にクライアントから送信）
{ "command": "cancel" }

// セッションリセット（内部状態クリア、新しい発話の受付再開）
{ "command": "reset" }
```

```rust
#[async_trait]
pub trait SttSession: Send {
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()>;
    async fn recv_event(&mut self) -> Result<Option<SttEvent>>;
    async fn close(&mut self) -> Result<()>;

    /// 現在の認識をキャンセル（割り込み時にクライアントから呼び出し）
    async fn cancel(&mut self) -> Result<()>;

    /// セッションをリセット（内部状態クリア）
    async fn reset(&mut self) -> Result<()>;
}
```

---

## 24. レビューフィードバック反映: 無音タイムアウト

### 24.1 概要

VC内で一定時間（デフォルト5分）無音が続いた場合、リソース節約のためWorkerを自動停止する。STT WebSocketセッションを閉じ、次の発話検知時に再起動する。

### 24.2 設計

```rust
const DEFAULT_SILENCE_TIMEOUT: Duration = Duration::from_secs(300); // 5分

impl PipelineWorker {
    async fn run_stream(
        &self,
        user_id: u64,
        mut audio_rx: mpsc::Receiver<Vec<f32>>,
        playback_tx: mpsc::Sender<PlaybackCommand>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let mut session = self.stt.connect().await?;
        let silence_timeout = self.config.silence_timeout
            .unwrap_or(DEFAULT_SILENCE_TIMEOUT);
        let mut last_speech_at = Instant::now();

        loop {
            tokio::select! {
                Some(pcm) = audio_rx.recv() => {
                    session.send_audio(&pcm).await?;
                }
                event = session.recv_event() => {
                    match event? {
                        Some(SttEvent::SpeechStart { .. }) => {
                            last_speech_at = Instant::now();
                            tracing::debug!(user_id, "Speech start (timer reset)");
                        }
                        Some(SttEvent::Final { text, .. }) => {
                            last_speech_at = Instant::now();
                            if text.trim().is_empty() { continue; }
                            self.process_text(user_id, &text, &playback_tx, &cancel).await?;
                        }
                        // ... 他のイベント処理 ...
                        None => break,
                        _ => {}
                    }
                }
                // 無音タイムアウトチェック
                _ = tokio::time::sleep_until(
                    tokio::time::Instant::from_std(last_speech_at + silence_timeout)
                ) => {
                    tracing::info!(
                        user_id,
                        timeout_secs = silence_timeout.as_secs(),
                        "Silence timeout reached, stopping worker"
                    );
                    break;
                }
                _ = cancel.cancelled() => break,
            }
        }

        session.close().await?;
        Ok(())
    }
}
```

### 24.3 設定

```toml
# config.toml
[voice.pipeline]
silence_timeout_secs = 300    # 無音タイムアウト（秒）。0=無効
```

### 24.4 再起動

Worker停止後に再度音声が検知された場合、Dispatcherが新しいWorkerを起動する（セクション7.2のDispatcher::handle_audioの既存ロジックで自動的に対応）。

---

## 25. レビューフィードバック反映: 同時参加上限と優先ロジック

### 25.1 概要

同時STTセッション上限（`max_concurrent_sessions`、デフォルト4）を超えた場合、**最後に話した人（Most Recently Spoken）**を優先するロジックを導入する。セクション17.3の単純な「5人目を無視」方式を拡張する。

### 25.2 LRS（Least Recently Spoken）追い出しロジック

上限超過時、最も長い間発話していないユーザーのSTTセッションを停止し、新しいユーザーにセッションを割り当てる。

```rust
use std::time::Instant;

/// ユーザーごとの最終発話時刻を追跡
struct UserActivity {
    user_id: u64,
    ssrc: u32,
    last_spoken_at: Instant,
}

impl Dispatcher {
    /// PCMチャンクを適切なWorkerセッションにルーティング（LRS追い出し付き）
    async fn handle_audio(&self, chunk: AudioChunk) {
        let ssrc = chunk.ssrc;

        // 既存セッションがあれば転送 + 発話時刻更新
        if let Some(tx) = self.user_sessions.get(&ssrc) {
            // 発話時刻を更新
            if let Some(mut activity) = self.user_activities.get_mut(&ssrc) {
                activity.last_spoken_at = Instant::now();
            }
            let _ = tx.send(chunk.pcm).await;
            return;
        }

        // 同時STTセッション数チェック
        if self.user_sessions.len() >= self.max_concurrent_stt {
            // LRS（最も長い間発話していない）ユーザーを特定
            let lrs_ssrc = self.find_least_recently_spoken();

            if let Some(lrs) = lrs_ssrc {
                tracing::info!(
                    evicted_ssrc = lrs,
                    new_ssrc = ssrc,
                    "Evicting LRS user to make room for new speaker"
                );
                self.evict_user(lrs).await;
            } else {
                tracing::warn!(ssrc, "Cannot evict any user, ignoring audio");
                return;
            }
        }

        // 新規ユーザーのセッション起動
        self.start_new_session(ssrc, chunk.pcm).await;

        // アクティビティ追跡に登録
        self.user_activities.insert(ssrc, UserActivity {
            user_id: ssrc as u64,
            ssrc,
            last_spoken_at: Instant::now(),
        });
    }

    /// 最も長い間発話していないユーザーのSSRCを返す
    fn find_least_recently_spoken(&self) -> Option<u32> {
        self.user_activities.iter()
            .min_by_key(|entry| entry.last_spoken_at)
            .map(|entry| entry.ssrc)
    }

    /// ユーザーのSTTセッションを停止・リソース解放
    async fn evict_user(&self, ssrc: u32) {
        // CancellationTokenでWorkerを停止
        if let Some((_, cancel)) = self.active_cancels.remove(&ssrc) {
            cancel.cancel();
        }
        self.user_sessions.remove(&ssrc);
        self.user_activities.remove(&ssrc);

        tracing::debug!(ssrc, "User evicted from STT session pool");
    }
}
```

### 25.3 追い出し通知

追い出されたユーザーには、テキストチャンネル経由で通知する（オプション）:

```toml
# config.toml
[voice.stt]
max_concurrent_sessions = 4
eviction_notify = true             # 追い出し時にテキストチャンネルで通知
eviction_cooldown_secs = 60        # 同一ユーザーへの通知間隔（スパム防止）
```

### 25.4 フローチャート

```
新しいユーザーの音声受信
    │
    ├── 既存セッションあり？ → YES → 転送 + last_spoken_at更新
    │
    └── NO → 上限超過？
              │
              ├── NO → 新規セッション起動
              │
              └── YES → LRS（最も無言が長い）ユーザーを特定
                         │
                         ├── LRSユーザーのWorkerを停止
                         ├── セッション枠を解放
                         └── 新しいユーザーのセッションを起動
```

---

## 26. VC連動テキスト投稿

> **ステータス: オプション機能（MVP対象外）**
> 音声対話の内容をテキストとしても残す利便性機能。MVP後に実装予定。

### 26.1 概要

Bot応答をTTSで音声チャンネルに再生すると同時に、VCに紐づくテキストチャンネルにもテキストとして投稿する。ユーザーのSTT結果（何を聞き取ったか）も投稿することで、音声対話の内容がテキストログとして可視化される。

### 26.2 投稿フォーマット

```
🎤 ユーザー名: STTで認識されたテキスト
🔊 Bot名: LLMの応答テキスト
```

例:
```
🎤 kojira: 今日の天気を教えて
🔊 LocalGPT: 東京は晴れ、最高気温15度の予報です。
```

### 26.3 設定（config.toml）

```toml
[voice.transcript]
enabled = false                    # VC連動テキスト投稿の有効/無効
channel_id = "1234567890"          # 投稿先テキストチャンネルID
post_stt = true                    # ユーザーSTT結果を投稿するか
post_tts = true                    # Bot応答テキストを投稿するか
```

### 26.4 設定構造体

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TranscriptConfig {
    /// VC連動テキスト投稿の有効/無効
    pub enabled: bool,                      // デフォルト: false
    /// 投稿先テキストチャンネルID
    pub channel_id: Option<ChannelId>,
    /// ユーザーSTT結果を投稿するか
    pub post_stt: bool,                     // デフォルト: true
    /// Bot応答テキストを投稿するか
    pub post_tts: bool,                     // デフォルト: true
}
```

### 26.5 データフロー

```
[STT完了]
    │
    ├── SttEvent::Final { user_id, text }
    │     │
    │     └── TranscriptWorker → テキストチャンネルに投稿
    │           「🎤 ユーザー名: STTテキスト」
    │
    ▼
[LLM応答完了]
    │
    ├── TTS再生開始
    │     │
    │     └── TranscriptWorker → テキストチャンネルに投稿
    │           「🔊 Bot名: 応答テキスト」
    │
    ▼
[割り込み発生時]
    │
    ├── 再生済み部分のみテキスト投稿
    │     「🔊 Bot名: 再生済みテキスト…（割り込み）」
    │
    └── 未再生分は投稿しない
```

### 26.6 割り込み時の部分テキスト処理

TTSの応答テキストをチャンク単位で管理し、再生済みチャンクのみを投稿する:

```rust
/// 割り込み時の部分テキスト生成
fn build_interrupted_text(chunks: &[TextChunk], last_played_idx: usize) -> String {
    let played_text: String = chunks[..=last_played_idx]
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("");
    format!("{}…（割り込み）", played_text)
}
```

### 26.7 TranscriptWorker

テキスト投稿を非同期で処理する専用Worker:

```rust
pub enum TranscriptEntry {
    UserStt {
        user_name: String,
        text: String,
    },
    BotResponse {
        bot_name: String,
        text: String,
    },
    BotResponseInterrupted {
        bot_name: String,
        played_text: String,
    },
}

/// TranscriptWorkerはmpscチャンネルでエントリを受信し、
/// Discord APIでテキストチャンネルに投稿する
pub struct TranscriptWorker {
    rx: mpsc::Receiver<TranscriptEntry>,
    config: TranscriptConfig,
    http: Arc<Http>,
}
```

### 26.8 モックTTSとの統合

§20.8で述べたとおり、モックTTSプロバイダもTranscriptWorkerを通じてテキストチャンネルに投稿可能。これにより、実際のTTSサーバーなしでもパイプライン全体のフロー（STT→LLM→TTS→テキスト投稿）をテキストベースで検証できる。

---

*本設計書はLocalGPT Voice v3.2の実装開始にあたっての基盤設計である。セクション19-25はv3e（テスト・モック・レビューフィードバック反映）として2026-02-13に追加された。セクション26はv3f（VC連動テキスト投稿・モックTTS修正）として2026-02-13に追加された。実装の進行に伴い更新される。*

