# Archtecture

## クレート構成

![](architecture.png)

---

リンク先は各クレートのREADMEです

### プラグインホスト抽象化層

- [plugin-host-api](../crates/plugin-host-api/README.md)
- [host-window](../crates/host-window/README.md)
- [vst3-host](../crates/vst3-host/README.md)
- [vst3-host-view](../crates/vst3-host-view/README.md)
- [clap-host](../crates/clap-host/README.md)
- [plugin-host](../crates/plugin-host/README.md)

VST3とCLAPを共通化します。

`WebGPU` 規格や `winit` のAPIを参考に、 `plugin-host-api` で全て抽象化しています。各プラグイン規格でAPI実装を行い、 `plugin-host` にすべて集め、公開しています。
結果として、ノードグラフもCLIもプラグイン規格に依存せず、将来新しい規格が増えても変更は plugin-host の分岐一箇所で済みます。

### 入れ子プラグイン汎用

- [subhost-adapter](../crates/subhost-adapter/README.md)

入れ子プラグインのための機能群です。

### audio-graph実装

- [audio-graph-settings](../crates/audio-graph-settings/README.md)
- [audio-graph-engine](../crates/audio-graph-engine/README.md)
- [audio-graph-plugin](../crates/audio-graph-plugin/README.md)

`audio-graph-engine` がグラフのコンパイル、実行準備、評価順序を担当します。
コンパイル結果の `Program` は外部から書き換えられず、`ProgramPublisher` が遅延リングを準備して `PreparedProgram` を公開します。`Engine::run_block` がスケジュール初期化とノート入力、パラメーターと音声のステージ実行をまとめて所有します。

`audio-graph-plugin` がエディターと `nice-plug` を使ったプラグイン梱包を担当します。実行時にはDAWの音声・イベント・transportを変換し、ホストされたプラグインの処理器をエンジンへ接続します。

AudioGraph固有の設定ファイル、探索フォルダー、ピン留め、カタログの保存先は `audio-graph-settings` が所有し、製品とCLIで共有します。`plugin-host` は呼び出し側から渡された探索対象とカタログ保存先を扱います。ブラウザー上の表示分類は `audio-graph-plugin` が決定します。

### 開発用CLIツール

- [host-cli](../crates/host-cli/README.md)

プラグインは「DAW に読み込んで手で操作する」以外の検証手段を持ちにくい領域です。
そこで、DAW を立ち上げずに実物のプラグインを相手に検証できる CLI を同梱しています。
引数のプラグインは `.vst3` でも `.clap` でもかまいません。

```sh
cargo run -p host-cli -- <コマンド>
```

| コマンド                                                          | 内容                                       |
| ----------------------------------------------------------------- | ------------------------------------------ |
| `dirs`                                                            | 実際に探索する設置場所と設定ファイルの位置 |
| `scan [DIR...]` / `info` / `params` / `buses`                     | 見つかったプラグインの中身を列挙           |
| `render <PLUGIN> <IN.wav> <OUT.wav>` / `synth <PLUGIN> <OUT.wav>` | 音を出す                                   |
| `graph` / `chain` / `instrument` / `sidechain` / `delay`          | グラフの振る舞いを検査する                 |
| `churn` / `twice` / `state` / `sweep` / `probe` / `gui`           | 寿命まわりを痛めつける                     |

`cargo run -p host-cli -- --help` で全コマンドが出ます。
