# STT サーバー: whisper-large-v3-mlx-4bit + VAD（別リポジトリ）

## 方針

- **STT サーバーは別リポジトリで実装する**。localgpt は依存関係を持たない。
- localgpt 側は既存の WebSocket STT プロバイダ ([`src/voice/provider/stt/ws.rs`](../src/voice/provider/stt/ws.rs)) と設定 `voice.stt.ws.endpoint` で接続するだけ。**このリポジトリに Python コードや STT サーバー用サブディレクトリは追加しない。**

---

## localgpt 側で行うこと

1. **ドキュメント**
   - STT サーバーが別リポジトリである旨と、プロトコル仕様への参照を記載する（例: `docs/voice-design.md` または `docs/voice-stt-ws-protocol.md`）。
   - 設定例: `config.example.toml` の `voice.stt.ws.endpoint` に、外部サーバー接続例を記載（既存であればそのまま）。
2. **コード変更**
   - なし。ws クライアントは既にプロトコルに対応済み。

---

## 別リポジトリで実装する STT サーバー

以下は **別リポジトリ**を用意する際の仕様メモ。localgpt はこのリポジトリに依存しない。

### プロジェクトの場所

- **パス**: `/Volumes/1TB/dev/projects/stt-server-mlx-whisper`
- 上記ディレクトリにプロジェクトを作成する。

### モデル

- **whisper-large-v3-mlx-4bit**（例: `path_or_hf_repo="mlx-community/whisper-large-v3-mlx-4bit"`）。

### VAD

- **Silero VAD** で発話区間を検出し、発話区間の PCM のみを Whisper に渡す。
- 16kHz PCM、32ms チャンク（512 サンプル）で `VADIterator` 使用を想定。

### プロトコル（localgpt の ws クライアントと合わせる）

- **受信**: 1) JSON config（`type`, `sample_rate`(16000), `channels`, `encoding`(pcm_s16le), `language`, `interim_results`, `temperature`）。2) PCM s16le バイナリフレーム。3) `{"type":"end_of_stream"}`。
- **送信**: JSON イベント `speech_start` / `partial` / `final` / `speech_end`。形式は [`src/voice/provider/stt/ws.rs`](../../src/voice/provider/stt/ws.rs) の `WsServerMessage` および `parse_server_message` が解釈する形に合わせる。

### 実装・ドキュメント

- 言語: Python 3.10+。
- 依存: mlx-whisper, silero-vad, websockets 等。
- そのリポジトリの README に起動方法と、localgpt の `voice.stt.ws.endpoint` 設定例を記載する。

---

## まとめ

- STT サーバーは **別リポジトリ**。localgpt に依存を持たせない。
- localgpt では **ドキュメント**（別リポジトリ・プロトコル参照・設定例）の追加のみ行う。
