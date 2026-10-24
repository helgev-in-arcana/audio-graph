# Architecture

AudioGraph は、プラグイン形式を扱うホスト層、入れ子のプラグインを管理する層、
製品固有のグラフ・エディター・設定に分かれています。
ホストライブラリの利用手順は [XPlatform Plugin Hosting](xplatform-plugin-hosting.md)、
各クレートの契約と検証方法は以下のREADMEを参照してください。

## クレート構成

![](architecture.png)

この画像には `plugin-module`、`plugin-scan`、`audio-graph-settings` が含まれていません。
現行の責務と依存関係は以下の説明を参照してください。

### プラグインホスト抽象化層

| クレート | 責務 |
| --- | --- |
| [plugin-host-api](../crates/plugin-host-api/README.md) | 音声バッファ、パラメーター、イベント、transport、ホストのサービス、所有権の共通型。依存クレートを持たない。 |
| [plugin-module](../crates/plugin-module/README.md) | 共有ライブラリのロード、バンドル内のバイナリ解決、モジュールのスレッド占有、ロード済み表、ファイルの列挙。 |
| [host-window](../crates/host-window/README.md) | 形式に依存しない子エディター用コンテナ、キー転送、タイマーとファイル記述子の監視。 |
| [vst3-host](../crates/vst3-host/README.md) | VST3のロード・処理・状態・コールバックと共通APIとの変換。 |
| [vst3-host-view](../crates/vst3-host-view/README.md) | VST3のビューとコンテナウィンドウを組み合わせ、接続・サイズ変更・破棄順を管理。 |
| [clap-host](../crates/clap-host/README.md) | CLAPのロード・処理・状態・拡張機能・GUIと共通APIとの変換。 |
| [plugin-host](../crates/plugin-host/README.md) | 指定された1つのモジュールについて形式を選び、共通の `Plugin` とホストAPIを公開。 |
| [plugin-scan](../crates/plugin-scan/README.md) | 指定フォルダーの列挙、スキャンカタログ、保存された参照の候補解決。 |

`vst3-host` と `clap-host` は `plugin-module` のロード部品を共有し、
`plugin-scan` もそのファイル列挙を直接利用します。探索先のフォルダーは呼び出し側が渡します。
既知のパスをロードするだけなら `plugin-host` で足り、`plugin-scan` への依存は不要です。

`vst3-host-view` は `vst3-host` に依存し、インスタンスとモジュールの寿命を保持する
`Vst3View` を受け取ります。コンテナとビューを一緒に所有することで、ビューを外してから
ウィンドウやプラグインを破棄する順序を守ります。

新しい形式を追加する場合は、共通APIを実装するバックエンドを用意して `plugin-host` に統合します。
上位のグラフやadapterが形式ごとの処理を持たずに済むようにする境界です。

### 入れ子プラグイン汎用

- [subhost-adapter](../crates/subhost-adapter/README.md)

DAWへ公開するスロットと子プラグインのパラメーターの対応、パラメーター値のスケジュール、
子プラグインの状態・通知・出力イベントの識別、処理区間に合わせたtransportの転送を担当します。
`AudioInstances` がグラフ側との境界で、グラフはどのインスタンスをいつ実行するかを決めます。

`subhost-adapter` は `plugin-host` に依存し、探索処理やAudioGraph固有の設定は持ちません。
保存状態の復元では、呼び出し側が渡す候補解決コールバックを使います。
プラグインを見つけられない場合も、参照・状態・スロットの対応を保持します。

### AudioGraphの製品実装

- [audio-graph-settings](../crates/audio-graph-settings/README.md)
- [audio-graph-engine](../crates/audio-graph-engine/README.md)
- [audio-graph-plugin](../crates/audio-graph-plugin/README.md)

`audio-graph-engine` は音声・MIDI・パラメーターのグラフをコンパイルして実行します。
子プラグインの実行は `subhost-adapter::AudioInstances` を通じて呼び出します。
エディター用のノード操作部品は `ui` feature に分かれ、CLIはUIなしの構成を利用します。

`audio-graph-plugin` はDAWへの公開、エディター、製品の容量上限、保存データを担当します。
`nice-plug` と `nice-plug-egui` を使ってVST3/CLAPとして動作し、
DAWの音声・イベント・transportをエンジンと子プラグインへ接続します。

設定ファイル、探索フォルダー、ピン留め、カタログの保存先は `audio-graph-settings` が管理し、
製品とCLIで共有します。カタログの作成・参照解決は `plugin-scan`、
ブラウザーの表示分類は `audio-graph-plugin` の責務です。

## メインスレッドとオーディオスレッド

### ネイティブプラグインの寿命

`plugin-host-api::Processor` は、処理に必要なインスタンス・モジュール・コールバックの寿命を保持します。
停止は `processor.deactivate()` でそのprocessorを消費し、別のインスタンスへ返却する操作はありません。

`MainThread<T>` とprocessorのネイティブ資源は所有スレッドで破棄します。
別スレッドからの返却は事前確保した記録へ保存し、所有スレッドが `reclaim_main_thread()` で回収します。
通常終了ではprocessorの返却と回収を済ませ、プラグインやビューを解放してからスレッド初期化ガードを破棄します。
所有スレッドが先に終了した場合、別スレッドに残った資源を任意のスレッドで破棄することはしません。

### グラフの公開と実行

1. 編集側の `Graph` をコンパイルし、外部から書き換えられない `Program` を作ります。
2. `ProgramPublisher` が遅延リングやカスタム処理器の状態をオーディオスレッド外で準備し、
   `PreparedProgram` を公開します。
3. オーディオスレッドの `Engine` が公開済みプログラムを採用し、対応するノードや遅延ラインの状態を引き継ぎます。
4. `Engine::run_block` がスケジュール初期化、ノート入力、パラメーター・MIDI・音声のステージ実行を統括します。
   退役したプログラムと状態は公開側へ戻して破棄します。

処理は中核の命令とノード側のカスタム処理器を併用します。
Key Param Select は `ParamKernel`、Audio Tremolo と MIDI Delay は型付きのカスタム処理器を使います。
処理設定と実行中の状態を分け、状態はノードの識別子と処理器の型に対応させて引き継ぎます。
設計判断と測定結果は [実行方式のADR](../dev/adr/2026-10-17_ir-dyn-hybrid-execution/README.md) を参照してください。

`Granularity` はパラメーターを計算する `resolution` と、音声ループを処理する `quantum` を分けて持ちます。
フィードバックループの外にある音声ステージはホストブロック単位で処理でき、
パラメーターの精度を変えてもすべての子プラグインを同じ区間長で呼ぶ必要はありません。
resolutionを変更する場合は、そのイベント量に合わせて子プラグインの処理器も準備し直します。

### 製品側の再構成

I/Oやパラメーター対応が変わるときは、製品側がprocessorの停止、再設定、プログラム公開を統括します。
processor構成に必要な公開番号を記録し、ブロック開始時にその番号以降のプログラムを採用できた場合だけ実行します。
公開番号は `ProgramPublisher` 内で単調増加し、`Engine::publication()` が採用済み番号を返します。
構成を変えない値の編集ではprocessorを再生成せず、オーディオ用mutexも取得しません。

## 開発と検証

- [host-cli](../crates/host-cli/README.md)

`host-cli` はDAWを起動せずに実物のプラグインを検証する、ソースツリー内の開発用ツールです。
配布ZIPにCLIの実行ファイルは含まれません。リポジトリ直下から次の形で実行します。

```sh
cargo run -p host-cli -- <コマンド> <引数...>
```

| コマンドの例 | 内容 |
| --- | --- |
| `dirs` / `scan` / `info` / `params` / `buses` | フォルダー、クラス、パラメーター、バスを調べる。 |
| `render` / `synth` / `automate` | 音声・音源・パラメーター変更を処理する。 |
| `graph` / `chain` / `instrument` / `sidechain` / `delay` | 公開されたAudioGraphバンドルの配線と処理を確認する。 |
| `tremolo` / `resolution` | MIDI・音声Tremoloの刻みと、パラメーター精度ごとのイベント容量を確認する。 |
| `gui` / `editor` / `live` | エディターや、LFOで値が動くパッチを開く。 |
| `churn` / `twice` / `state` / `sweep` / `probe` | ロード・状態保存・再生成・破棄を繰り返して確認する。 |

全31コマンドの分類と実行条件は [host-cli README](../crates/host-cli/README.md#running-commands) を参照してください。
引数なしで実行するとusageを表示して終了コード1を返します。専用の `--help` 処理はありません。
通常のプラグイン引数は `.vst3` / `.clap` に対応しますが、wrapperを使う検証は各コマンドの指定形式に従います。

ホストの試験には [CLAP fixture](../crates/clap-test-plugin/README.md) と
[VST3 fixture](../crates/vst3-test-plugin/README.md) を使います。
別パッケージのcdylibを先に用意するため、workspaceをビルドしてからテストします。

```sh
cargo build --workspace --locked
cargo test --workspace --locked
cargo test -p audio-graph-engine --locked
```

最後の試験はUIを有効にしないエンジン構成の確認です。workspace全体ではfeatureが統合されるため、別途実行します。
Linuxのウィンドウ試験にはXサーバーが必要で、CIでは `xvfb-run -a cargo test --workspace` を使います。
任意のインストール済みプラグインを使う試験は [`.env.example`](../.env.example) の明示的なパス指定に従います。
