# Nodes

`AudioGraph.vst3` / `AudioGraph.clap` 1ファイルにインストゥルメント、エフェクト両方入っています。

|                        |                    |
| ---------------------- | ------------------ |
| Audio Graph Instrument | インストゥルメント |
| Audio Graph FX         | エフェクト         |

AddNodeもしくは空白を右クリックでノードを配置

Plugin foldersでプラグインのスキャンディレクトリを登録

設定はユーザーごとの領域（Windows は `%LOCALAPPDATA%\AudioGraph\config.json`、
macOS は `~/Library/Application Support/AudioGraph/`、
Linux は `$XDG_CONFIG_HOME/audio-graph/`）に保存され、同じPC内で共有されます。

ノード操作はBlenderのノードを参考にしています。

- Parameter resolution — パラメーターの値を何サンプルごとに計算し、サブプラグインへ送るか。
  1 / 4 / 16 / 32 / 64 / 128 から選び、既定は 32（48kHz で約0.67ms）。
  小さいほど滑らかですが、動いている値はその間隔ごとに1イベントになります。
  オーディオの処理単位（Sub-block）とは独立しています。
- Sub-block — オーディオのフィードバックループを含むパッチが一度に処理するサンプル数で、
  オーディオ遅延の最短時間でもあります。16 / 32 / 64 / 128、既定は 32。
  小さいほどループ内のプラグインが頻繁に呼ばれます。

拍の長さ（LFO の rate の beats、MIDI Delay の sync 時の time）は DAW と同じく分数で指定します。
`[分子] / [分母▾] [3]` の形で、分子は 1〜64 の整数、分母は 1 / 2 / 4 / … / 64 から選び、
[3] で3連符（長さが 2/3）になります。単位は拍なので、4分の4拍子で 1/4 は16分音符です。

## Audio

ソケットは青、サイドチェインが紫
<br>色分けしていますが交互に繋げられます

- Audio Delay
- Audio Out
- Mix
- Audio Gate
- Audio Math
- Audio Tremolo

## MIDI

ソケットは黄色
<br>フルチャンネル、MIDI-CC込み

- MIDI In
- MIDI Gate
- Key MIDI Route
- MIDI Split
- MIDI Key Mute
- MIDI Filter
- MIDI Merge
- MIDI Delay
- MIDI Tremolo
- Param -> CC
- CC In
- Velocity
- Gate
- Key Track
- Held Keys

## Parameter

ソケットは緑

- Envelope Follower
- Constant
- Slot In
- LFO
- Math
- Param Map
- Param Select
- Key Param Select

## Plugin Node

- [+] でパラメーター入力を呼び出せます。
- [GUI] でプラグインのUIが開きます
- [always on] でプラグインが常に有効になります。デフォルトでは出力が繋がっていないプラグインの処理が省略されるので、アナライザなど、出力無しであっても動かしたいプラグインは [always on] で有効にしてください。
