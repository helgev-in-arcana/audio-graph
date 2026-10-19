# Core OpとCustom KernelによるHybrid実行

状態: **採用済み**。調査開始: 2026-10-17。製品移行: 2026-10-18。追加検証: 2026-10-19。記録整理: 2026-10-20。

対象は`refactor/hybrid-keyparam`の`fe088b8`まで。この記録は当時の判断と根拠を保存するもので、将来の実装仕様を自動的に更新する文書ではない。

## 背景

KeyParamを細かい命令列へ展開するコンパイル処理と、特殊ノードごとに中央の命令・評価分岐・状態管理を増やす構造が、理解と変更の負担になっていた。共通命令の性能上の利点を残しつつ、特殊ノードの実装を局所化することを目的とする。

## 決定と代案

**共通処理はCore Opに残し、KeyParam・AudioTremolo・MidiDelayをCustom Kernelへ移す。** 今後の特殊ノードも、既存の契約で表現できる限りKernelと状態をノード側へ置く。通常のNodeKind登録は必要だが、中央の専用Op・評価分岐・状態配列を追加しない。

| 選択肢 | 判断 |
| --- | --- |
| 専用Opと細かな命令への展開を維持 | 小さい処理の固定費を抑えられるが、今回減らしたいコンパイル・レビューの負担が残る |
| Core Op + Custom | 採用。特殊処理を局所化し、配線・Filter・Merge・基本演算などの共通処理を維持できる |
| 全面的にdynへ移行 | 将来の選択肢として残す。今回の製品計測では全ノードdyn化の性能を判断できない |

2候補KeyParamや動作中Audioの一部には小さい絶対時間の増加が残る。それを避けるためのノード別二重実装やunsafe化は採らない。8候補KeyParam、停止中Audio、高頻度のNote行処理では改善があり、実装を一箇所へ集約する利益と合わせて採用した。数値・測定条件・限界は[調査記録](investigation.md)に置く。

## 構造と守る契約

- コンパイラは到達性・順序・stage・配線・バッファ割当を引き続き管理する。ノードのコンパイルは、不変の設定を持つKernelとローカルな入出力の対応表を登録する。Custom命令は通常の命令列・stageに入る。
- ParamのKernelはProgramが所有し、状態はEngineのラッチに置く。入力はコピーを集めない読み取りビュー、出力は専用の`&mut [f64]`、状態は任意の1ラッチ、Note入力は接続先の読み取りビュー。Kernelから全レジスタや他ノードの状態へはアクセスできない。
- Audio / NoteのKernelは関連型`State`を持つ。共通adapterが設定と状態を保持し、評価ごとのAny downcastは行わない。採用時にNodeIdと具体型を照合し、**状態だけ**交換して新設定を使う。確保・旧runtimeの破棄は非実時間側で行う。
- Audioは同じチャネル幅の1入力・1出力をin-placeまたは別領域として借用する。Noteは1入力・追記専用の容量制限付き出力を借用し、時刻・lane等は行単位で共有する。dyn呼び出しはCustomの評価単位で、音声の各サンプルには置かない。
- ノード固有のreset・保留ノート解放は状態側へ集約し、所有管理と終了通知は共通側と分担する。`Engine::release`は既存poolと同様に現在の状態をEngineへ残す。プリセット再読込のリセット方針は別件とする。

## 変更範囲と読み始める場所

製品変更は`crates/audio-graph-engine`内。`crates/audio-graph-plugin`は統合テストの追加で、GraphのJSON形式・pluginの状態バージョンは維持した。

| 関心 | 主な実装 |
| --- | --- |
| Paramの契約と借用 | [param_kernel.rs](../../../crates/audio-graph-engine/src/ir/param_kernel.rs)、[param_context.rs](../../../crates/audio-graph-engine/src/engine/param_context.rs) |
| Audio / Noteの契約と状態採用 | [kernel.rs](../../../crates/audio-graph-engine/src/ir/kernel.rs)、[audio_kernel.rs](../../../crates/audio-graph-engine/src/ir/audio_kernel.rs)、[note_kernel.rs](../../../crates/audio-graph-engine/src/ir/note_kernel.rs)、[kernel_runtime.rs](../../../crates/audio-graph-engine/src/engine/kernel_runtime.rs) |
| コンパイルの登録・接続 | [cx.rs](../../../crates/audio-graph-engine/src/compile/cx.rs)、[notes.rs](../../../crates/audio-graph-engine/src/compile/notes.rs) |
| ノード固有の意味と実装 | [key_param.rs](../../../crates/audio-graph-engine/src/nodes/key_param.rs)、[audio_tremolo.rs](../../../crates/audio-graph-engine/src/nodes/audio_tremolo.rs)、[note_delay.rs](../../../crates/audio-graph-engine/src/nodes/note_delay.rs) |

alpha段階のリポジトリ内互換性を優先し、未使用の`Program: Clone + PartialEq`と`PreparedProgram: PartialEq`を外した。`Op::Custom`の追加、`AudioOp::Tremolo`・`NoteOp::Delay`の置換も外部Rustコードにはソース互換性の変更になる。

## 再検討する条件

- Audio / Noteの多入力・生成専用処理、Paramの複数状態が必要になったらContextを見直す。先回りした汎用化は行わない。
- 同じ状態型でも設定によって容量・形が変わるノードでは、状態移行の適合性を判定する必要がある。設定でNote streamの意味が変わるノードでは、型・node・port・上流に加えて設定をidentityへ含める設計が必要になる。
- Custom状態の採用はO(n²)の照合。256個で約10.5µsという今回の結果を、大規模Graphの上限保証には使わない。任意Kernelの実時間安全性も型だけでは保証できず、実装ごとの検証が必要。
- CoreとCustomの比率や新規ノード追加の負担が変わったら、全面dyn化を再評価する。KeySwitch、MIDI Tremolo、Mixの最適化、極小値対策は今回の採用範囲に含めない。

## 記録の置き方

- [investigation.md](investigation.md): 比較方法、結果、検証、既知の課題、再現情報。
- [data/keyparam](data/keyparam/): CPU固定の最終比較と前後の生ログ。[data/audio-note](data/audio-note/): 最終3組の生データ・比較表と採用時間の集計。
- 再利用する実Engine計測は[hybrid_keyparam.rs](../../../crates/audio-graph-engine/examples/hybrid_keyparam.rs)と[hybrid_audio_note.rs](../../../crates/audio-graph-engine/examples/hybrid_audio_note.rs)をexamplesに残す。
- 先行実験の別backend・一時実装は`exp/ir-vs-dyn-benchmark`の`5c535b4`を参照する。このディレクトリには複製しない。不採用の試作と中間ログを残し続ける運用にはしない。
- 統合前の設計・計画・報告・中間ログは`fe088b8`の`docs/hybrid-*.md`と`docs/benchmarks/hybrid-*`から復元できる。履歴は書き換えず、READMEのサブページ・アセットと作業記録を分離する。
