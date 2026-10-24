<div align="center">

# AudioGraph

**A _node-based_ Instrument / Effect plugin
for VST3 and CLAP _sub_ plugins**

**中でプラグインを読み込んで、ノードベースで音を弄れるプラグイン**<br>
**音、パラメーター、MIDIのすべてをノードで編集できます。**

![](./docs/example-screenshot.png)

</div>

## Why AudioGraph?

DTMで、ディレイループにエフェクトを掛けることは、伝統的なトラックベースの音声処理では不可能です。

また、不可能ではない表現でも、複雑なエフェクトや複雑な音声ルーティングは見にくく作業しづらい。

ノードなら全て解決できます。一発で音声やパラメーターの流れを理解でき、作業しやすく、表現力も完全上位互換。
マクロやLFOなど、手持ちのプラグインに存在しない機能を外から取り付けることもできます。

音源/エフェクト問わず、複数のプラグインを組み合わせて大きなマクロを組むことも可能。

## Download / Getting Started

[Latest Version(`v0.1.6-alpha`)](https://github.com/helgev-in-arcana/audio-graph/releases/tag/v0.1.6-alpha)

[All Releases](https://github.com/helgev-in-arcana/audio-graph/releases)

`AudioGraph.vst3` / `AudioGraph.clap` の1ファイルに、インストゥルメント（`Audio Graph Instrument`）とエフェクト（`Audio Graph FX`）の両方が入っています。

対応状況

| OS(Arch)                       | vst | clap | load vst | load clap |
| ------------------------------ | --- | ---- | -------- | --------- |
| Win-11(x86_64)                 | ✅  | ✅   | ✅       | ✅        |
| Win-11(arm)                    | ⚠️  | ⚠️   | ⚠️       | ⚠️        |
| Ubuntu-24.04(XWayland)(x86_64) | -   | ✅   | ✅       | ✅        |
| Ubuntu-24.04(-)(arm)           | -   | ⚠️   | ⚠️       | ⚠️        |
| Mac                            | x   | x    | x        | x         |

機能、詳しい操作とノードの一覧は →[Nodes and Usage](docs/nodes-usage.md)

## Build from source

Toolchain: Rust 1.98.0 (`rust-toolchain.toml`) / MSRV: Rust 1.95.0

```sh
cargo xtask bundle audio-graph-plugin --release --locked
```

`target/bundled/` に `AudioGraph.vst3` と `AudioGraph.clap` ができます。

## [Architecture](docs/architecture.md)

[![](docs/architecture.png)](docs/architecture.md)

## plugin-host — xPlatformプラグインホストライブラリ

副産物として、**Rust から VST3 / CLAP プラグインをホストするためのライブラリ**が
`plugin-host` クレートとして手に入ります。形式は拡張子から決まり、コードに形式名は現れません。

```rust
use plugin_host::{Plugin, SubPluginMain, SubPluginProcessor};

let mut plugin = Plugin::load(path, None, host_context)?;   // .vst3 でも .clap でも同じ
let mut processor = plugin.activate(config)?;
processor.process(&mut buffers, &events, &time, &mut out_events);
processor.deactivate();
```

詳しい利用手順は →[XPlatform Plugin Hosting](docs/xplatform-plugin-hosting.md)<br>
現時点では API は AudioGraph の内部利用が主で、安定していません。

## Roadmap

→[Roadmap](docs/roadmap.md)

→[Issues](https://github.com/helgev-in-arcana/audio-graph/issues)

## Notice / 注意

🚧🚧🚧―――――――――――――――――――

- 現在アルファ版です。任意のリリースに破壊的変更の可能性が含まれます。
- UIは現在仮組みです。改良中です。
- コード署名は現在ありません。
- MacOS版、arm版が動作未確認です。
  - Win11(x86_64)、Ubuntu24(x86_64)はVST3/CLAPともに動作確認しています。
  - **macOS** はビルドが通るだけで、ウィンドウ実装がまだありません。
  - 手持ちのVST3とCLAPで動作確認していますが、規格のすべてをテストできている保証は無いです。
- VST2ネイティブ対応はライセンスの問題で今後も不可能です。VST3化ツールなど使ってください。
- このリポジトリはAI駆動で開発されていますが、私([@helgev-in-arcana](https://github.com/helgev-in-arcana))が設計・コード監査を行っています。<br>
  §Architecture など、クレート/モジュール構成、依存フローの構造は確認/リファクタが完了しています。

―――――――――――――――――――🚧🚧🚧　　　

## License

MIT OR Apache-2.0

配布バイナリに組み込まれる依存ライブラリとフォントの著作権表示は
[THIRD-PARTY-NOTICES.md](./THIRD-PARTY-NOTICES.md) にまとめてあります。

VST is a trademark of Steinberg Media Technologies GmbH, registered in Europe
and other countries.
<br>本プロジェクトは Steinberg Media Technologies GmbH と
提携・関連するものではありません。
