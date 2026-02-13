# LocalGPT Voice 実装ステップ

**プロジェクト:** LocalGPT Voice  
**設計書:** [voice-design.md](voice-design.md) (v3.2)  
**作成日:** 2026-02-13  

---

## 依存関係グラフ

```
Step 1 (骨格)
  ├── Step 2 (VC接続)
  │     ├── Step 3 (モック接続)
  │     │     ├── Step 4 (テキスト返信)
  │     │     │     └── Step 5 (TTS返し)
  │     │     │           └── Step 6 (文分割+並列TTS)
  │     │     │                 └── Step 7 (割り込み)
  │     │     │                       └── Step 8 (マルチユーザー)
  │     │     └─────────────────────────────┐
  │     └── Step 9 (3体Bot E2Eテスト) ←── Step 2〜8 各ステップで並行実施
  │
  └── Step 10 (本番STT) ← Step 3完了後いつでも着手可
```

---

## Step 1: 骨格作成

**依存:** なし（最初のステップ）

### やること

1. `src/voice/` モジュールディレクトリを作成（設計書§9のディレクトリ構造に従う）
   - `mod.rs` — VoiceManager 初期化・ライフサイクル管理（スタブ）
   - `gateway.rs` — songbird standalone driver連携（スタブ）
   - `receiver.rs` — VoiceReceiveHandler（スタブ）
   - `dispatcher.rs` — Dispatcher（スタブ）
   - `worker.rs` — PipelineWorker（スタブ）
   - `agent_bridge.rs` — VoiceAgentBridge（スタブ）
   - `audio.rs` — PCM変換・リサンプリングユーティリティ（スタブ）
   - `provider/mod.rs` — SttProvider, TtsProvider trait定義（設計書§4）
   - `provider/stt/mod.rs`, `provider/stt/ws.rs` — STTプロバイダ（スタブ）
   - `provider/tts/mod.rs`, `provider/tts/aivis_speech.rs` — TTSプロバイダ（スタブ）
2. `lib.rs` に `#[cfg(feature = "voice")] pub mod voice;` を追加
3. `Cargo.toml` に feature flag と依存を追加（設計書§10）
   - `voice` feature: `songbird`, `rubato`, `hound`, `tokio-tungstenite`
4. `config.toml` に `[voice.*]` セクションを追加（設計書§8）
   - `[voice]`, `[voice.discord]`, `[voice.pipeline]`, `[voice.stt]`, `[voice.stt.ws]`, `[voice.tts]`, `[voice.tts.aivis_speech]`, `[voice.agent]`, `[voice.audio]`, `[voice.transcript]`
5. `src/config/` に VoiceConfig 構造体を追加（Deserialize対応）

### 完了条件

- `cargo build --features voice` が警告のみ（エラーなし）で通る
- `cargo build`（デフォルト、voice無し）も引き続き通る
- 各スタブモジュールがコンパイルされる

### テスト方法

```bash
# feature無しビルド
cargo build 2>&1 | grep -c "error" # → 0

# voice featureビルド
cargo build --features voice 2>&1 | grep -c "error" # → 0

# trait定義が正しくコンパイルされることを確認
cargo test --features voice --lib voice::provider 2>&1
```

---

## Step 2: VC接続

**依存:** Step 1

### やること

1. **songbird standalone driver 初期化**（設計書§6.1）
   - `VoiceManager::new()` で `Arc<Songbird>` を生成（serenity不要）
   - Bot user IDを設定から取得
2. **VC join/leave コマンド実装**
   - 既存Discord GatewayにVoice State Update (op=4) 送信機能を追加
   - `VoiceManager::join(guild_id, channel_id)` / `leave(guild_id)`
3. **既存Discord Gatewayとのイベント受け渡し**（設計書§6.2）
   - `src/discord/mod.rs` にVoice State Update / Voice Server Update のハンドラを追加
   - `#[cfg(feature = "voice")]` ガード付き
   - Voice State Update → `session_id` 保存
   - Voice Server Update → `ConnectionInfo` 構築 → songbird接続開始
4. **Opus→PCM受信**（設計書§6.3）
   - `VoiceReceiveHandler` を songbird に登録
   - Opus→PCM変換（48kHz stereo → 16kHz mono リサンプリング、rubato使用）
   - 受信パケットをログ出力（SSRC, サンプル数, RMS）
5. **VC接続状態機械**（設計書§22）
   - `VcState` enum: `Disconnected` → `Connecting` → `Connected` → `Reconnecting`
   - 状態遷移のログ出力
   - 再接続ロジック（exponential backoff）

### 完了条件

- Botが指定VCに参加できる（Discordクライアントで緑のアイコン表示）
- Botが指定VCから離脱できる
- VCで誰かが喋ると、音声パケット受信がログに出力される
- VC接続状態が `Connected` になることをログ確認
- ネットワーク切断時に `Reconnecting` → `Connected` に自動復帰

### テスト方法

```bash
# Botを起動してVC参加
cargo run --features voice -- daemon
# → ログに "Voice connected via songbird standalone driver" が出る

# VCで発話
# → ログに "Received audio: ssrc=XXXXX, samples=960, rms=0.XX" が出る

# Bot離脱コマンド（HTTP API or CLI）
# → ログに VcState: Connected → Disconnected が出る
```

**ユニットテスト:**
- `VcState` 状態遷移テスト（設計書§22のテストコード）
- リサンプリング品質テスト（48kHz→16kHz、正弦波で確認）

---

## Step 3: モック接続

**依存:** Step 2

### やること

1. **モックSTTプロバイダ実装**（設計書§19）
   - `MockSttProvider` / `MockSttSession`: PCM受信→固定テキスト or エコーテキスト返却
   - RMS ベースの疑似VAD（閾値超えで `SpeechStart` → `Final` → `SpeechEnd`）
   - 設定可能なレイテンシシミュレーション
   - `SttEvent` の全イベント種別を返却（`SpeechStart`, `Partial`, `Final`, `SpeechEnd`）
2. **モックTTSプロバイダ実装**（設計書§20）
   - `MockTtsProvider`: テキスト→無音PCM or サイン波PCM生成
   - テキスト長に比例した再生時間シミュレーション
   - 設定可能なレイテンシシミュレーション
3. **パイプライン疎通**
   - Dispatcher → PipelineWorker の mpsc チャンネル接続
   - Worker内: モックSTT → テキスト取得 → モックTTS → PCM生成
   - 生成PCMを songbird 経由で VC に送信（無音/サイン波が聞こえる）

### 完了条件

- モックSTTがPCM入力に対して `SttEvent::Final` を返す
- モックTTSがテキスト入力に対して PCM を返す
- VCで喋ると、パイプライン全体が流れることをログで確認:
  `Audio received → STT Final: "mock text" → TTS synthesized: 1200ms → Audio sent`
- モックモードで VC にサイン波/無音が再生される

### テスト方法

```bash
# config.toml でモックプロバイダを指定
# [voice.stt] provider = "mock"
# [voice.tts] provider = "mock"

cargo run --features voice -- daemon
# VCで発話 → ログでパイプライン全体のフロー確認

# ユニットテスト
cargo test --features voice -- voice::provider::stt::mock
cargo test --features voice -- voice::provider::tts::mock
```

**ユニットテスト（設計書§21）:**
- モックSTT: PCMフレーム送信→イベント順序検証（`SpeechStart` → `Partial`* → `Final` → `SpeechEnd`）
- モックTTS: テキスト→PCMサンプル数・再生時間の検証
- パイプライン結合テスト: PCM入力→テキスト出力→PCM出力の一連フロー

---

## Step 4: テキスト返信

**依存:** Step 3

### やること

1. **STT Final → テキストチャンネル投稿**
   - PipelineWorker が `SttEvent::Final` を受信したら、LLM応答を生成
   - VoiceAgentBridge 経由で Agent を呼び出し（直接アクセス、設計書§4.3）
   - LLM応答テキストを Discord テキストチャンネルに投稿
2. **VC連動テキスト投稿**（設計書§26）
   - `TranscriptWorker` 実装: mpsc で `TranscriptEntry` を受信→テキストチャンネルに投稿
   - ユーザーSTT結果: `🎤 ユーザー名: 認識テキスト`
   - Bot応答: `🔊 Bot名: 応答テキスト`
   - `[voice.transcript]` 設定で有効/無効切り替え
3. **音声は返さない**（このステップではTTS再生はスキップ）

### 完了条件

- VCで喋った内容がSTTで認識され、LLM応答がテキストチャンネルに投稿される
- 投稿フォーマットが `🎤` / `🔊` 形式
- `[voice.transcript] enabled = false` で投稿が無効になる

### テスト方法

```bash
# config.toml: モックSTT + transcript enabled
cargo run --features voice -- daemon

# VCで発話 → テキストチャンネルに投稿確認
# 🎤 kojira: (STT認識テキスト)
# 🔊 LocalGPT: (LLM応答テキスト)
```

**ユニットテスト:**
- VoiceAgentBridge: テキスト入力→応答テキスト生成（Agent直接呼び出し）
- TranscriptWorker: エントリ投稿フォーマット検証

---

## Step 5: TTS返し

**依存:** Step 4

### やること

1. **AivisSpeech TTSプロバイダ実装**（設計書§5.3）
   - `AivisSpeechProvider`: REST API (`/audio_query` → `/synthesis`)
   - config.toml の `[voice.tts.aivis_speech]` からエンドポイント・スタイルID等を取得
   - PCM出力（24kHz）→ リサンプリング（48kHz）→ Opus encode → songbird 送信
2. **1文のみ、割り込み無しのシンプル再生**
   - LLM応答テキスト全体を1回のTTS呼び出しで合成
   - 合成完了後にVCへ再生
   - 再生中の割り込みは無視（Step 7で対応）
3. **再生キュー**
   - Worker → Main への PCM 送信用 mpsc チャンネル
   - songbird の `Track` として PCM を再生

### 完了条件

- VCで喋ったら Bot が音声で返答する
- AivisSpeech が正常に音声合成し、VCで聞こえる
- 音質が自然（リサンプリングアーティファクトなし）

### テスト方法

```bash
# AivisSpeech を起動（ローカル:10101）
# config.toml: [voice.tts] provider = "aivis-speech"

cargo run --features voice -- daemon
# VCで「こんにちは」→ Botが音声で応答

# レイテンシ計測: 発話終了→応答開始 < 3秒（目標2秒、この段階では未最適化OK）
```

**ユニットテスト:**
- AivisSpeechProvider: HTTP モック → PCM 出力の検証
- リサンプリング: 24kHz→48kHz 品質テスト（正弦波）

---

## Step 6: 文分割＋並列TTS

**依存:** Step 5

### やること

1. **LLMストリーミング応答の句読点分割**（設計書§16.1）
   - `SentenceSplitter`: LLMストリームトークンを蓄積し、句読点（`。！？\n`）で分割
   - 日本語・英語の句読点対応
   - 最小文長閾値（短すぎる文は次の文と結合）
2. **セグメント並列TTS生成**（設計書§16.2）
   - 文が確定するたびに即座にTTS合成を開始（`tokio::spawn`）
   - 最大並列数制限（Semaphore、デフォルト3）
   - 順序保証: 文の順番通りに再生キューに投入
3. **順序保証再生**（設計書§16.3）
   - `SequencedPlaybackQueue`: 文番号付きPCMを受け取り、順番通りに再生
   - プリバッファ: 最初の文のTTS完了後すぐに再生開始
4. **TTSキャッシュ**（設計書§16.5）
   - SQLite BLOB ストア（テキスト+スタイルID → PCM）
   - LRU eviction（最大サイズ設定）
   - キャッシュヒット時はTTS呼び出しスキップ

### 完了条件

- 長い応答（3文以上）が文単位で逐次再生される（全文合成完了を待たない）
- 最初の文の再生開始が、全文一括合成時より明らかに速い
- 文の順序が正しい（1文目→2文目→3文目の順に再生）
- 同じテキストの2回目以降はキャッシュヒットし、TTS呼び出しが発生しない

### テスト方法

```bash
cargo run --features voice -- daemon

# VCで「今日の天気と明日の予定を教えて」など長い応答を誘発
# → 最初の文が先に再生され、後続が順次再生されることを確認
# → ログに "TTS cache hit" / "TTS cache miss" が出る
```

**ユニットテスト:**
- SentenceSplitter: 各種句読点パターンの分割テスト
- SequencedPlaybackQueue: 順序保証テスト（文2が先に完了→文1完了まで待機→順番再生）
- TTSキャッシュ: ヒット/ミス/eviction テスト

---

## Step 7: 割り込み

**依存:** Step 6

### やること

1. **Barge-in検知**（設計書§16.4）
   - Bot再生中にユーザーの `SttEvent::SpeechStart` を検知
   - 再生中フラグの管理（`AtomicBool`）
2. **再生停止＋LLMキャンセル**
   - songbird の `Track::stop()` で即座に再生停止
   - LLMストリーミングの `CancellationToken` をキャンセル
   - TTS合成キューの残りを破棄
3. **会話履歴の部分記録**（設計書§26.6）
   - 再生済みチャンクのテキストのみ会話履歴に記録
   - 未再生分は破棄（`…（割り込み）` 表記でTranscript投稿）
4. **無音タイムアウト**（設計書§24）
   - VCに参加後、5分間音声入力がなければ Worker を停止
   - タイムアウト時間は設定可能（`[voice.pipeline] idle_timeout_sec = 300`）
   - タイムアウト時のログ出力とリソース解放

### 完了条件

- Bot発話中にユーザーが話したら、Botの再生が即停止する（< 200ms）
- LLMの生成もキャンセルされる（不要なトークン生成が止まる）
- 停止後、ユーザーの新しい発話に対して新たに応答を開始する
- 会話履歴に再生済み部分のみ記録される
- 5分無音で Worker が自動停止する

### テスト方法

```bash
cargo run --features voice -- daemon

# Bot応答中に「ストップ」と発話 → 即停止確認
# ログに "Barge-in detected, cancelling playback" が出る
# ログに "Idle timeout (300s), stopping worker" が出る（5分放置で確認）
```

**ユニットテスト:**
- Barge-in検知: `SpeechStart` イベント→再生停止の状態遷移テスト
- CancellationToken: キャンセル後のストリーム終了テスト
- 無音タイムアウト: タイマーテスト（短いタイムアウト値で検証）

---

## Step 8: マルチユーザー

**依存:** Step 7

### やること

1. **最大4人同時STT**（設計書§17）
   - ユーザーごとに独立した STT セッション（WebSocket接続）を維持
   - `MAX_CONCURRENT_STT = 4` のリソース上限（Semaphore）
   - 5人目以降はキューイング or 拒否
2. **SSRC→UserIDマッピング**（設計書§17.2）
   - Voice State Update からユーザー情報を取得
   - songbird の SSRC をユーザーIDに紐付け
   - マッピング更新イベントの処理
3. **コンテキストウィンドウ方式mix**（設計書§18）
   - 2秒のウィンドウで複数ユーザーの発話を蓄積
   - ウィンドウ満了時にバッチで LLM に送信
   - フォーマット: `[ユーザーA]: テキスト\n[ユーザーB]: テキスト`
4. **LRS (Least Recently Spoken) 優先ロジック**（設計書§25）
   - 同時参加上限到達時、最も長く喋っていないユーザーの STT を一時停止
   - 新しいユーザーが喋り始めたら、LRS ユーザーの STT セッションを解放

### 完了条件

- 複数人（2〜4人）が VC で喋って、Bot がグループ会話として応答する
- 各ユーザーの発話が正しく識別される（SSRC→UserIDマッピング）
- 複数人が同時に喋った場合、コンテキストウィンドウで統合されて LLM に送られる
- 5人目が参加した場合、LRS ユーザーの STT が一時停止される

### テスト方法

```bash
cargo run --features voice -- daemon

# 2人以上でVCに参加して会話
# → 各ユーザーの発話がSTTで正しく認識される
# → Bot がグループ会話として応答（「kojiraさんの質問に答えると…」のような文脈理解）
# → ログに "Context window batch: 2 users, 3 utterances" のような出力
```

**ユニットテスト:**
- SSRC→UserIDマッピング: マッピング登録・更新・削除テスト
- コンテキストウィンドウ: タイミングベースのバッチ集約テスト
- LRS優先: 同時接続上限時の優先度テスト

---

## Step 9: 3体Bot E2Eテスト

**依存:** Step 2〜8（各ステップで並行して実施）

### やること

1. **3体Botの準備**
   - **ほわちゃん**: LocalGPT Voice 本体（テスト対象）
     - Token: `data/secrets/howari_discord_token.txt`
   - **のすたろう**: ユーザー発話役（事前録音音声を再生）
     - Token: OpenClaw の Discord token
   - **らぼみ**: 検証役（応答を聞いて検証）
     - Token: OpenClaw の Discord token
2. **テスト用VCの準備**
   - テスト専用のDiscordサーバー/VCチャンネルを使用
   - 3体全員を同じVCに参加させる
3. **E2Eテストシナリオ（ステップ別）**
   - **Step 2 E2E**: 3体がVCに参加/離脱できる
   - **Step 3 E2E**: ほわちゃんがモックパイプラインで応答（サイン波再生）
   - **Step 4 E2E**: のすたろうの発話→ほわちゃんがテキストチャンネルに応答
   - **Step 5 E2E**: のすたろうの発話→ほわちゃんが音声で応答
   - **Step 6 E2E**: 長い応答が逐次再生される
   - **Step 7 E2E**: のすたろうが割り込み→ほわちゃんが停止
   - **Step 8 E2E**: のすたろう＋らぼみが同時発話→ほわちゃんがグループ応答
4. **テスト用の複雑なモックは作らない** — 実際の3体のBotで実環境テスト
5. **事前録音音声の準備**
   - のすたろう/らぼみが再生する音声ファイル（Opus）を用意
   - テスト用セリフ: 「こんにちは」「今日の天気は？」「ストップ」等

### 完了条件

- 3体が VC で自然に対話できる
- 各ステップの E2E テストが Pass
- ほわちゃんが のすたろうの発話を認識し、音声で応答する
- らぼみが応答を受信・検証できる

### テスト方法

```bash
# 3体を起動
# ほわちゃん: cargo run --features voice -- daemon
# のすたろう: 専用テストスクリプト（songbird で事前録音再生）
# らぼみ: 専用テストスクリプト（音声受信・録音・検証）

# テスト実行
# のすたろうが「こんにちは」を再生
# → ほわちゃんがSTT→LLM→TTS→応答
# → らぼみが応答を録音し、音声が存在することを検証
# → テキストチャンネルにTranscriptが投稿されることを検証
```

---

## Step 10: 本番STTサーバー接続

**依存:** Step 3（モックSTTが動いていれば着手可能）

### やること

1. **VoxtralベースSTTサーバー確認**
   - A100サーバー上の既存STTサーバーのWebSocketエンドポイント確認
   - プロトコル互換性確認（設計書§4.1.1のWebSocketプロトコル）
2. **WebSocket STTプロバイダ実装**（`provider/stt/ws.rs`）
   - `WsSttProvider` / `WsSttSession`: WebSocket接続確立
   - PCM f32 LE をバイナリフレームで送信
   - JSON テキストフレームで `SttEvent` を受信・パース
   - 再接続ロジック（設計書§12のエラーハンドリング）
3. **config.toml切り替え**
   - `[voice.stt] provider = "ws"` + `[voice.stt.ws] endpoint = "ws://..."` 設定
   - モックSTT ↔ 本番STT の切り替えが設定変更のみで可能
4. **レイテンシ計測**
   - 発話終了→STT Final の所要時間を計測
   - 設計書§11のレイテンシバジェット（STT: ~500ms）を満たすか確認

### 完了条件

- リアルタイム音声認識が動作する（VCで喋った内容が正しくテキスト化される）
- 日本語の認識精度が実用レベル
- STT レイテンシが 500ms 以下（5秒の発話に対して）
- ネットワーク断時に自動再接続する

### テスト方法

```bash
# STTサーバーが稼働していることを確認
wscat -c ws://stt-server:8766/ws

# config.toml: [voice.stt] provider = "ws"
cargo run --features voice -- daemon

# VCで日本語発話 → ログに正確なSTT結果が出る
# レイテンシ: "STT Final latency: XXXms" のログ確認

# ネットワーク断テスト: STTサーバーを再起動
# → "STT reconnecting..." → "STT reconnected" のログ確認
```

**ユニットテスト:**
- WsSttProvider: WebSocket接続・切断・再接続テスト（ローカルモックサーバー使用）
- SttEvent パース: 各JSONフォーマットの正しいデシリアライズ

---

## 全体タイムライン（目安）

| ステップ | 見積もり | 累計 |
|---|---|---|
| Step 1: 骨格作成 | 1日 | 1日 |
| Step 2: VC接続 | 3日 | 4日 |
| Step 3: モック接続 | 2日 | 6日 |
| Step 4: テキスト返信 | 2日 | 8日 |
| Step 5: TTS返し | 2日 | 10日 |
| Step 6: 文分割+並列TTS | 3日 | 13日 |
| Step 7: 割り込み | 2日 | 15日 |
| Step 8: マルチユーザー | 3日 | 18日 |
| Step 9: 3体Bot E2E | 並行実施 | — |
| Step 10: 本番STT | 2日（Step 3後いつでも） | — |

**合計:** 約18日（Step 9は並行、Step 10は独立）

---

## 参照

- 設計書: [voice-design.md](voice-design.md)
- §4: Provider Trait設計
- §6: Discord Voice接続
- §8: 設定ファイル
- §9: ディレクトリ構造
- §10: Cargo.toml
- §16: TTS応答パイプライン拡張
- §17: マルチユーザーSTT
- §18: コンテキストウィンドウ方式mix
- §19: モックSTTプロバイダ設計
- §20: モックTTSプロバイダ設計
- §21: テストコード設計
- §22: VC接続状態機械
- §24: 無音タイムアウト
- §25: 同時参加上限と優先ロジック
- §26: VC連動テキスト投稿
