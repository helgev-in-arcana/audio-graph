# Nodes

`AudioGraph.vst3` や `AudioGraph.clap` 1つにインストゥルメント、エフェクト両方入っています。

AddNodeもしくは空白を右クリックでノードを配置します。

Plugin foldersでプラグインのスキャンディレクトリを登録します。

設定はユーザーごとの領域（Windows は `%LOCALAPPDATA%\AudioGraph\config.json`、
macOS は `~/Library/Application Support/AudioGraph/`、
Linux は `$XDG_CONFIG_HOME/audio-graph/`）に保存され、同じマシンの全インスタンスで共有されます。

ノード操作はBlenderのノードを参考にしています。

## Audio

ソケットは青、サイドチェインが紫

色分けしていますが交互に繋げることができます

- Audio I/O
- Mix
- Audio Gate

## MIDI

ソケットは黄色

- MIDI In
- MIDI Gate
- Key MIDI Route
  - 切り替えに使うキーは既定で取り除かれます（[mute switching keys]）。

## Parameter

ソケットは緑

- Constant
- LFO
- Expression
- Math
- Param Map
- Param Select
- Key Param Select
  - notes 出力から入力したMIDIをそのまま流せます。値を選ぶキーは既定で取り除かれ（[mute picking keys]）、外すと下流にも送られて発音します。

## Plugin

- [+] でパラメーター入力を呼び出せます。
- [GUI] でプラグインのUIが開きます
- [always on] でプラグインが常に有効になります。デフォルトでは出力が繋がっていないプラグインの処理が省略されるので、アナライザなど、出力無しであっても動かしたいプラグインは [always on] で有効にしてください。
