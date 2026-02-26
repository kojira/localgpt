## 現状整理（GPT相談用）

### 前提・背景

- **プロジェクト**: LocalGPT（Rust）。ローカルで動くAIアシスタントで、音声入力（STT）に対応している。
- **技術スタック**: localgpt 本体は Rust。STT の一方式として「STT Browser Bridge」があり、Node.js + Puppeteer で Chrome を起動し、Web Speech API で認識させる。
- **アーキテクチャ**: localgpt が PCM（16kHz モノ s16le）を WebSocket でブリッジに送る → ブリッジが Chrome のページに渡す → ページで `SpeechRecognition` を動かし、partial/final を localgpt に返す。
- **ビジネス上の目的**: ユーザーが「enumerateDevices を上書きする方式は無理なので、仮想デバイスを実際に作って」と指示。つまりブラウザ API を偽装せず、OS レベルで実在する仮想デバイスに音声を送り、そのデバイスを getUserMedia で取得して認識する形にしたい。

### これまでの経緯

1. 当初はページ側で `enumerateDevices` / `getUserMedia` を上書きし、仮想的な「virtual-audio」デバイスを返し、PCM を AudioContext で再生してそのストリームを認識に渡していた。
2. ユーザーから「上書きは無理。仮想デバイスを実際に作って」と指摘。
3. **Linux**: PulseAudio の `pactl load-module module-null-sink` で null-sink `stt_bridge` を実際に作成。PCM を `paplay -d stt_bridge` で再生。ページはその sink の monitor を getUserMedia で取得。**この経路では認識が動く想定。**
4. **macOS**: OS 側で仮想デバイスをこちらから作成できないため、**PCM フォールバック**を実装。ページが PCM を受け取り、AudioContext → MediaStreamDestination に再生し、その MediaStreamTrack を `SpeechRecognition.start(track)` に渡す（enumerateDevices の上書きはしていない）。
5. ユーザー gesture 要件のため「Start STT」ボタンを設け、トラックが live になったら `track_live` でサーバーに通知し、Puppeteer でボタンクリックしてから `recognition.start(track)` を実行。
6. それでも Chrome が `onerror` で **`not-allowed`** を返し、認識結果が得られない。

### 試した対策と結果

- クリックハンドラ内で `recognition.start(track)` を呼ぶように変更 → 依然 `not-allowed`。
- トラックが live になってからクリック（`track_live` 受信後にクリック）→ 同上。
- クリックハンドラ内で `audioContext.resume()` を実行してから `start(track)` → 同上。
- クリック時に一度 `getUserMedia({ audio: true })` で許可を取得してから合成トラックで `start(track)` → 同上。
- ログでは「recognition.start(track) called (PCM stream, user gesture)」の直後に `onerror error=not-allowed` が発生。

### 目的

- **Linux**: 実仮想デバイス（PulseAudio null-sink）で音声認識が確実に動く状態を維持する。
- **macOS**: 送った PCM を Web Speech API で認識させたい。BlackHole 2ch + sox で実デバイス経由にすれば認識が動く。PCM フォールバック（sox 未使用）では Chrome が not-allowed を返すことがあるため、macOS では BlackHole + sox の利用をドキュメントで推奨する。

### 現在の状況

- 受け入れテスト `stt_browser_bridge_accepts_fixture` は **macOS でもパス**（接続・config・PCM・end_of_stream までエラーなし）。ただし認識イベントは 0 件（空の partial 1 件のみ、onerror 経由）。
- Linux + PulseAudio では、実デバイス経由のため認識が動く設計になっている（ユーザー環境で未検証の場合は「想定」）。
- README に「Linux では実デバイスで認識可能」「macOS では PCM フォールバックで Chrome が not-allowed を返すことが多い」と記載済み。

### 問題

- **macOS で Chrome が `SpeechRecognition.start(MediaStreamTrack)` に渡した「合成トラック」（AudioContext の createMediaStreamDestination 由来）に対して `not-allowed` を返す。** ユーザー gesture 内で start しても解消していない。
- 想定している原因候補: (1) Chrome が合成トラックを SpeechRecognition の入力として許可していない、(2) Puppeteer による自動操作が「ユーザー gesture」とみなされていない、(3) Chrome 133+ の start(track) が実マイクトラックと合成トラックで扱いが異なる。

### 関連コード/ファイル

- ブリッジ: `stt-browser-bridge/server.js`（WebSocket サーバー、config に useRealDevice を付与、track_live 受信でボタンクリック）
- ページ注入: `stt-browser-bridge/inject.js`（useRealDevice が false のとき PCM を queuePcm で再生、MediaStreamDestination の track で recognition.start、Start STT ボタンで gesture 内 start）
- 仮想デバイス: `stt-browser-bridge/virtual-device.js`（Linux で pactl により null-sink 作成、paplay で再生）
- テスト: `src/voice/provider/stt/ws.rs` 内 `stt_browser_bridge_accepts_fixture`
- 説明: `stt-browser-bridge/README.md`

### 制約条件

- **Web Speech API をどうしても使う必要がある。** Whisper 等のローカル STT やクラウド STT への切り替えは不可。Web Speech API の枠内で解決策を探したい。
- enumerateDevices / getUserMedia の**上書きは使わない**（ユーザー指示）。
- macOS では OS の仮想デバイスをスクリプトから作成する手段がない（BlackHole 等はユーザーが手動導入する前提）。
- Web Speech API は headless では動かないため、Puppeteer は headless: false で Chrome を起動している。

---

## GPT への質問

1. **Chrome の `SpeechRecognition.start(MediaStreamTrack)` で、AudioContext の `createMediaStreamDestination()` 由来のトラックが `not-allowed` になる既知の制限・仕様はありますか？** 公式ドキュメントや Chromium の issue で心当たりがあれば教えてください。

2. **Puppeteer 等の自動操作下で Web Speech API が「ユーザー gesture」を認めず `not-allowed` を返す事例や、回避策（フラグ・起動オプション・実マイクを一度開く等）はありますか？**

3. **Web Speech API を必ず使う前提で**、macOS で「ブラウザに送った PCM を認識させる」を実現する方法はありますか？（Whisper 等への切り替えは不可。Chrome の起動オプション・権限・実デバイス経由・PCM の渡し方など、Web Speech API の範囲内で可能な手立てを教えてください。）

4. 上記を踏まえ、macOS で BlackHole + sox 以外の経路（PCM フォールバック）で Web Speech API の認識が難しい場合、ドキュメント・テストでは「macOS では BlackHole 2ch と sox を用意し、再生先・マイクを BlackHole に設定すれば認識できる」と明記する方針で問題ないでしょうか？ その場合のテストの書き方のアドバイスも欲しいです。
