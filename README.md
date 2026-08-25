# AudioGraph

中でプラグインを読み込んで、ノードベースで音を弄れるプラグインです。

音の配線、パラメーターの配線、MIDIの配線すべてノードで編集できます。

🚧🚧🚧―――――――――――――――――――――――――――――――――――――――

- 現時点のコミット先頭は全てバイブコードされた状態です。
  - 私(@helgev-in-arcana)が現在コード・設計監査を進めています。
- 今はアルファ版で、動作確認ができているのは Windows (x86_64) のみです。読み込めるプラグインは VST3 と CLAP です。
  - リリースには Windows (x86_64 / arm64)、Linux (x86_64 / arm64)、macOS (Intel / Apple Silicon) のビルドが並びますが、
    **Windows (x86_64) 以外は一切動作確認できていません**。ビルドが通っているというだけの状態です。
  - CLAP読み込みは自前のテスト用CLAPプラグインでの検証のみで、市販CLAPでの動作確認は未実施です。
  - VST2ネイティブ対応はライセンスの問題で不可能です。VST3化ツールなど使ってください。
- UIは現在仮組みです。改良予定です。ゲインが現在dBではなく実数値倍率になっています。
- 現在アルファ版です。任意のリリースに破壊的変更の可能性が含まれます。
- コード署名が現在ありません。

―――――――――――――――――――――――――――――――――――――――🚧🚧🚧

![](./docs/example-screenshot.png)

## 現時点でできること

- Fx と Instrument の 2 クラスを 1 バイナリから提供
- **VST3 と CLAP の両方を読み込める**（同じグラフの中で混在できます）
- プラグイン読み込みは16個まで
- 音声、MIDI、パラメーター値のノードベース編集
- DAWからのオートメーションスロットが32本
- 自動遅延保証対応

## 現時点でできないこと

- Win以外の対応 (Win11(x86_64)のみ動作確認)
- VST3 / CLAP 以外のプラグインの読み込み
- AudioGraph 自身を VST3 以外として書き出すこと
- プラグインプロセス分離
- 32bitプラグイン読み込み
- 出力バスを 2 本以上持つプラグインのソケットは表示されますが、実機での検査は未実施です。
- 出力バスを 1 本も宣言しないプラグインは、オーディオのコンパイル対象から外れます。

## 把握しているバグ

- ディレイノードでディレイ量が変化したときに波形の補間がうまくいっていない
- ほか、存在するであろう未発見のバグ

見つけた場合はIssueまで。
一方で、現在アルファ版ですので開発によってバグが直ったり増えたり復活したりする可能性が0ではありません。

## リリース

https://github.com/helgev-in-arcana/audio-graph/releases

Windows (x86_64 / arm64)、Linux (x86_64 / arm64)、macOS (Intel / Apple Silicon) 向けのアセットを配布しています。
ただし **Windows (x86_64) 以外は未検証**です。手元に環境がなく動作確認が取れていないため、
読み込めない・落ちるといったことは十分にあり得ます。動いた／動かなかったの報告は歓迎します。

## License

MIT OR Apache-2.0

配布バイナリに組み込まれる依存ライブラリとフォントの著作権表示は
[THIRD-PARTY-NOTICES.md](./THIRD-PARTY-NOTICES.md) にまとめてあります。

VST is a trademark of Steinberg Media Technologies GmbH, registered in Europe
and other countries. 本プロジェクトは Steinberg Media Technologies GmbH と
提携・関連するものではありません。

---

❗以下Claudeによる自動生成❗

---

## ビルドと導入

Rust（edition 2024 が通る版）が必要です。

```sh
# 1. ビルドしてバンドルに包む
#    target/bundled/ に AudioGraph.vst3 と AudioGraph.clap ができます。
cargo xtask bundle audio-graph-plugin --release

# 2. DAW の VST3 フォルダへ配置
#    Windows: C:\Program Files\Common Files\VST3\
#    Linux:   ~/.vst3/
#    macOS:   ~/Library/Audio/Plug-Ins/VST3/
```

CLAP として使う場合は `AudioGraph.clap` を CLAP フォルダ（Windows は
`C:\Program Files\Common Files\CLAP\`、Linux は `~/.clap/`、macOS は
`~/Library/Audio/Plug-Ins/CLAP/`）へ置きます。どちらを出すかは xtask が決めるの
ではなく、組み上がったバイナリがどのエントリポイントをエクスポートしているかで
決まります。

DAW を再スキャンすると、`Audio Graph FX` と `Audio Graph Instrument` の 2 つが現れます。

### 最初の一手

1. トラックに AudioGraph を挿してエディタを開く
2. キャンバスに **Plugin ノード**を置き、手持ちの VST3 か CLAP を読み込む（フィルタ欄に `clap` と打つと形式で絞れます）
3. `AudioIn` → Plugin → `AudioOut` を繋ぐ（ここまでで普通のプラグインとして音が通ります）
4. **LFO ノード**を置き、その出力を Plugin ノードのパラメータソケットへ繋ぐ
5. LFO の rate を拍同期にすると、テンポに追従して揺れます

### プラグインが一覧に出てこないとき

VST3 にも CLAP にも「DAW がどこを探しているか」を訊く手段がないので、慣例的な
設置場所以外はこちらからは分かりません。

エディタ上部の **Plugin folders** を開いてフォルダを追加してください。初回起動時に
その OS の慣例的な設置場所が書き込まれており、以降はそこに並ぶフォルダが探索対象の
すべてです。慣例由来の行も含めて自由に削除できます（重い・壊れているフォルダを
外せます）。消しすぎたときは **Add the usual folders** で慣例パスが戻ります。

設定はプラグインの隣ではなくユーザーごとの領域（Windows は
`%LOCALAPPDATA%\AudioGraph\config.json`、macOS は
`~/Library/Application Support/AudioGraph/`、Linux は `$XDG_CONFIG_HOME/audio-graph/`）
に保存され、同じマシンの全インスタンスで共有されます。
`cargo run -p host-cli -- dirs` で、実際に探索しているフォルダを確認できます。

## ノードグラフの考え方

### ポートには型がある

| 型 | 運ぶもの |
|---|---|
| `Param` | 1 ブロックあたり 1 個の数値。パラメータを動かす線 |
| `Audio { channels }` | 音そのもの。チャンネル数が合わない線は繋がらない |
| `Note` | ノートイベント。行き先を明示しない限り誰にも届かない |

型が違うソケット同士は繋がりません。色分けされているので、繋げる先は見れば分かります。

### スロット — DAW との接点

グラフの内部は DAW を知りません。DAW から入ってくるオートメーションは
**スロット**（32 本）という決まった口を通り、`SlotIn` ノードとしてグラフに現れます。
逆に `SlotOut` ノードへ書き込むと、そのスロットに対する DAW のオートメーションを上書きできます。

スロットが実際にどのサブプラグインのどのパラメータへ繋がるかは、グラフの外側で束縛されます。
これによって、プラグインを差し替えてもグラフの形はそのまま残ります。

### 編集と実行は分かれている

キャンバスを触っている間、グラフは一時的に不整合であってよい ── 線が片方しか繋がっていない、
というのは編集の途中では当たり前です。一方でオーディオスレッドは、確保も解放もせず、
分岐も最小限で走る必要があります。この 2 つは同じデータ構造では両立しません。

```
  Graph  ──compile──▶  Program  ──Handoff──▶  Engine
  編集側                平坦・順序付き          オーディオスレッド
  可変・直列化可能       ・検査済み             確保しない、解放しない
```

`compile` がトポロジカルソートと型検査を行い、通れば実行可能な `Program` になります。
`Handoff` はロックを使わずに新しい `Program` を下へ渡し、古いものを上へ返します
（解放はメインスレッドで起きます）。

### ディレイと Mix

ディレイは `DelayWrite`（入力だけ・出力なし）と `DelayRead`（出力だけ・入力なし）の
2 つのノードに分かれていて、`line` の番号で対応づきます。線で繋がっていないので、
トポロジカルソートから見ると循環が存在しません。これがフィードバックの仕組みそのものです。
1 本の line を複数の `DelayRead` が読めば、そのままマルチタップになります。

入力ソケットは 1 本しか線を受けないので、合流は `Mix` ノードで明示的に行います。
ゲインも `Mix` が持ちます（入力 1 本の Mix はゲインそのものです）。

## アーキテクチャ

依存は上から下への一方向です。

```
audio-graph-plugin   nice-plug でのプラグイン書き出し、egui のノードエディタ
      │
audio-graph-engine   グラフ、コンパイラ、オーディオ／パラメータ評価器
      │                ★ プラグイン形式を一切知らない
subhost-adapter      入れ子であること固有の処理
      │                トランスポート伝播、レイテンシ合算、state の入れ子、スロット束縛
      │
plugin-host          マルチフォーマット・プラグインホスト（下 4 つのファサード）
      │                ├ plugin-host-api   依存ゼロの抽象。トレイトとデータモデル
      │                ├ vst3-host         VST3 バックエンド（純 Rust）
      │                ├ clap-host         CLAP バックエンド（純 Rust）
      │                └ host-window       形式を知らないコンテナウィンドウ
      │
host-cli             DAW の代役となる検査 CLI
```

**`audio-graph-engine` がプラグイン形式を知らないこと**が、この構成の要です。
数を読んで数を書き、バッファを読んでバッファを書くだけで、VST3 が何かも、
サブプラグインというものが存在することも知りません。おかげで CLAP 対応は純粋な追加になります。

**`subhost-adapter`** は「入れ子であること」だけを引き受けます。ここに置くかどうかの判断基準は、
「オフラインレンダラやプラグインスキャナにも必要か」── 必要ならば下の層へ、
不要ならばここへ、です。

**形式を知っている場所は `plugin-host` だけ**です。`subhost-adapter` から上は
`plugin-host` にしか依存せず、`vst3-host` や `clap-host` という名前を一度も書きません。
形式の分岐はトレイトオブジェクトではなく `enum Format` の `match` にしてあるので、
3 つ目の形式を足したときに書き忘れた腕はコンパイラが落とします。

## プラグインホストライブラリ

AudioGraph の副産物として、**Rust から VST3 / CLAP プラグインをホストするためのライブラリ**が
手に入ります。AudioGraph 本体とは独立に使えるように保ってあります。

### 使い方

利用者が依存するのは `plugin-host` 1 つです。形式は拡張子から決まり、
コードに形式名は現れません。

```rust
use plugin_host::{Plugin, SubPluginMain};

// .vst3 でも .clap でも同じ行で読める
let mut plugin = Plugin::load(path, None, host_context)?;

for p in SubPluginMain::params(&plugin) {
    println!("{}  {}..{}", p.name, p.min, p.max);   // plain 値。正規化しない
}

let mut processor = plugin.activate(config)?;        // オーディオスレッドへ渡せる半分
processor.process(&mut buffers, &events, &time, &mut out_events);
plugin.deactivate(processor);
```

走査は `plugin_host::installed_modules()`（両形式まとめて）と `scan_module(path)`。
エディタは `plugin.open_editor(owner)` / `close_editor()` / 毎フレームの `tick()` で、
VST3 の `IPlugView` と CLAP の GUI 拡張という構造の違いは外に出ません。

保存した束縛を復元するときは `PluginRef`（形式 + id + パスヒント + 表示名）を
`resolve_reference()` に渡します。id が権威でパスはヒントなので、
プラグインフォルダの違う機械でもプロジェクトが開きます。

### API の形

中心にあるのは `plugin-host-api` です。このクレートは**依存を 1 つも持ちません**。
それは意図的で、「バックエンド固有の型（COM ポインタ、生ポインタ、参照、`Arc`）が
公開シグネチャに漏れてはならない」という規約を、レビューではなくコンパイラに守らせるためです。
漏れたらビルドが通りません。

**2 つのトレイトでスレッドを分ける。**

```rust
pub trait SubPluginMain {                    // メインスレッド専用
    fn activate(&mut self, config: AudioConfig) -> Result<Box<dyn SubPluginProcessor>>;
    fn deactivate(&mut self, processor: Box<dyn SubPluginProcessor>);
    // パラメータ列挙、state の保存／復元、エディタ …
}

pub trait SubPluginProcessor: Send {         // オーディオスレッド専用
    fn process(&mut self, buffers: &mut AudioBuffers, /* … */) -> Result<ProcessStatus>;
}
```

`activate` が Processor を作って所有権を渡します。どちらのスレッドから何を呼べるかが、
規約ではなく型で表現されています。

**データモデルの方針。**

- パラメータは **plain 値 + レンジ**。0..1 の正規化ではありません
  （VST3 バックエンドが内部で正規化に落とします）。
- イベントは `ParamEvent::{SetValue, Modulate, GestureBegin, GestureEnd}` と
  `NoteEvent::{NoteOn, NoteOff, NoteEnd, Expression, Midi}`。
- `AudioBuffers` は**平坦表現**（`&mut [f32]` + チャンネル数 + フレーム数 + レイアウト）。
  入れ子スライスは使いません。
- エラーは所有型の平坦な enum。バックエンドの型を引きずりません。
- **モデルは意図的に CLAP 寄り**に作り、VST3 側が degrade して合わせます。
  2 つの形式の共通部分に狭めることはしません。

### 2 つの形式の違いをどう吸収しているか

- **バスの交渉。** VST3 は `setBusArrangements` で交渉できますが、CLAP のポートは
  宣言された幅そのものです。CLAP バックエンドは要求と宣言が食い違えば断ります
  （黙って詰め替えると、サイドチェインの半分が未初期化のまま渡るような壊れ方をするため）。
- **未結線のバス。** VST3 には「無効なバス」がありますが、CLAP にはありません。
  CLAP バックエンドは未結線の入力ポートに無音バッファ、余った出力ポートに捨てバッファを
  渡します（プラグインは宣言数ぶん全部読むので）。
- **エディタの寿命。** VST3 は `IPlugView` という別オブジェクト、CLAP はインスタンスの拡張。
  どちらも `Plugin` が内部に持ち、破棄順は `Drop` が保証します。
- **メインスレッドのポンプ。** CLAP はホストが `on_main_thread` とタイマーを回す必要が
  あります。`Plugin::tick()` がそれで、エディタが開いていなくても毎フレーム呼びます。

### 今後の予定

- **feature フラグ**で対応形式を付け外しできるようにする（現状は両方が常に入ります）
- macOS / Linux 対応。OS 依存は `host-window` にほぼ閉じています
- 市販 CLAP プラグインでの実機検証

現時点では API は AudioGraph の内部利用が主で、安定していません。

## 開発

```sh
cargo build --workspace   # 先に。cargo test は他パッケージの cdylib を作らないため
cargo test --workspace
```

CLAP バックエンドのテストは `clap-test-plugin`（ワークスペース内の本物の `.clap`）を
実際にロードして検査します。ビルドされていなければテストは**スキップせず落ちます** ──
「緑だが何も検査していない」を避けるためです。

### host-cli — DAW なしで確かめる

各段階の「DAW 上で動くこと」は、実際には `host-cli` で確認しています。
DAW を立ち上げずに、実物のプラグインを相手に検証できます。

```sh
cargo run -p host-cli -- <コマンド>
```

プラグインを指す引数は **`.vst3` でも `.clap` でもかまいません**。拡張子でバックエンドが
決まります。

**調べる**

| コマンド | 内容 |
|---|---|
| `dirs` | 実際に探索する設置場所と、その設定ファイルの位置を列挙（設定が無ければ慣例パスで作成する） |
| `scan [DIR...]` | 見つかったモジュールを全部ロードして中身を列挙 |
| `info <PLUGIN>` | 1 モジュールの詳細 |
| `params <PLUGIN>` | インスタンス化してパラメータを列挙 |
| `buses <PLUGIN>` | ノードグラフから見えるバス構成を表示 |

**音を出す**

| コマンド | 内容 |
|---|---|
| `render <PLUGIN> <IN.wav> <OUT.wav>` | プラグインにオーディオを通す |
| `synth <PLUGIN> <OUT.wav>` | インストゥルメントに 1 音入れる |

**グラフの振る舞いを検査する**

| コマンド | 内容 |
|---|---|
| `nest <WRAPPER.vst3>` | state からサブプラグインを復元できるか |
| `graph <WRAPPER.vst3> <IN.wav>` | グラフの LFO がサブプラグインまで届くか |
| `chain <WRAPPER.vst3> <IN.wav> <A> <B>` | A→B の経路が、A と B を順に通した結果と一致するか |
| `instrument <WRAPPER.vst3> <SYNTH> <A> <B>` | ノートが繋いだ相手にだけ届くか |
| `sidechain <WRAPPER.vst3> <COMP> <SYNTH> <ID>` | 別ノードの音でコンプがダッキングするか |
| `delay <WRAPPER.vst3>` | フィードバックディレイがブロックサイズによらず同じ音か |

**寿命まわりを痛めつける**

| コマンド | 内容 |
|---|---|
| `churn <PLUGIN> [N]` | ロード／アンロードを N 回（既定 1000） |
| `twice <PLUGIN> [N]` | 連続 N 回インスタンス化 |
| `state <PLUGIN>` | インスタンスをまたいでパラメータを保存／復元 |
| `sweep [DIR...]` | 全プラグインを 1 個ずつ子プロセスで寿命テスト |
| `probe <PLUGIN>` | 同じ寿命テストをこのプロセス内で |
| `gui <PLUGIN>` | エディタを開いて破棄（`--reverse` で破棄順を逆に） |
| `editor <WRAPPER.vst3> <PLUGIN>` | プラグインノードを置いた状態でエディタを開く |

**オーディオスレッドでの確保を検出する**

```sh
cargo build -p audio-graph-plugin --release --features assert_process_allocs
```

`process` の中で何かが確保したらプロセスを abort します。既定で無効なのは、
毎ブロックのコストがかかるためです。
