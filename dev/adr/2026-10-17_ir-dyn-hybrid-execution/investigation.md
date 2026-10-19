# Hybrid実行の調査・測定記録

[採用判断](README.md)の根拠をまとめる。数値は2026-10-18の製品移行時の測定、テスト結果は2026-10-19の追加検証まで。2026-10-20の文書整理・履歴整理では再測定や製品変更を行っていない。

## 対象の履歴

| 用途 | 保存点 |
| --- | --- |
| 先行調査の開始 | `276bc4b`、2026-10-17 |
| 先行実験の保管 | `exp/ir-vs-dyn-benchmark` / `5c535b4190adb683d2e6fd89055eb85b9d688f65` |
| 製品移行の起点 | `main` / `dbb70df6fded4d7f080622c371c729d55f80e37d` |
| KeyParam移行前の計測example付き | `4f9f020f45b55439de50b48b7066d039b59cd0df`。製品処理は起点と同じ |
| KeyParam移行後 / Audio・Note移行前 | `f732cbdeba8026391d95a312fa56b66531afcf9e` |
| Audio・Note移行後の計測ソース | `de0e9c2e08df389c66880162de55383b9e220099` |
| 追加テストの検証点 | `6a0be820aa350006097a2a261aba2df64b4690c1` |

製品移行以降の参照は履歴整理後のコミットへ更新し、計測・検証時の製品コードとexampleの内容が一致することを確認した。文書だけの保存点が除かれた箇所は、同じ製品状態を持つ直前のコミットを使う。先行実験と移行起点の履歴は変更していない。

先行実験には`crates/audio-graph-engine/src/execution_experiment/`、`crates/audio-graph-engine/examples/ir_*.rs`、`docs/ir-execution-choices.md`、`docs/ir-vs-dyn-performance-guidance.md`がある。方式を比較するための別実装であり、以下の実Engine計測とは区別する。試作コードは製品側へ持ち込まず、採用した考え方と最終比較を残す。

## 共通の測定条件と読み方

Windows x86_64、Intel Core i7-13700K、rustc 1.98.0（`88d9e12ae`、LLVM 22.1.8）、48 kHz / 120 BPM。通常のrelease設定で、追加のtarget-cpu / LTO指定なし。最終比較は論理CPU 2（affinity mask `4`）へ固定し、この作業のビルド・テストと並行実行していない。クロック固定やOSの干渉排除は行っていない。

公開APIの`Engine::run_block`と`end_block`を測り、状態をブロック間で継続する。Graph構築、compile、prepare / publish / adopt、検証用の記録とハッシュはブロック計時外。実PluginのDSP負荷は含まない。

64ブロックの予備実行から反復数を決め、1回約3 msの計測を21回行う。CSVの時間単位はns/block。median / p10 / p90は**各計測内の平均時間の分位点**であり、単一callbackの最大遅延、信頼区間、他CPUでの保証値ではない。小さい差にはコード配置やばらつきも含まれるが、繰り返し符号の揃う増加をすべて誤差とは扱わない。

## KeyParam

### 構成と手法

[hybrid_keyparam.rs](../../../crates/audio-graph-engine/examples/hybrid_keyparam.rs)で12本の独立したパラメータ出力経路を測る。DAWスロット32、入力値0.5、Plugin実体は`NoInstances`。ブロック計時にはスケジュール初期化、全パラメータ行、終端行のpeek、ノート経路、Plugin境界、台帳の終了処理を含む。

| fixture | 12経路の構成 | 移行前 ops / registers | 移行後 ops / registers |
| --- | --- | ---: | ---: |
| core | SlotIn → Multiply × 4 | 60 / 60 | 60 / 60 |
| key2 | 2候補KeyParam、Select / Toggle各6 | 54 / 36 | 24 / 24 |
| key8 | 8候補KeyParam、Select / Toggle各6 | 162 / 108 | 24 / 24 |
| mixed | 8候補Selectとcore各6 | 132 / 84 | 42 / 42 |
| key8_muted | key8の選択キーをnotes出力でミュート | 162 / 108 | 24 / 24 |

最初の候補だけSlotInに接続し、残りは既定値。全KeyParamは共通のNoteInを読む。最初のKeyParamのnotes出力をPluginへ接続し、coreではNoteInを直接接続する。

64 / 512 frames × resolution 32 / 4 / 1 × idle / periodic / denseの90条件。periodicは毎ブロックkey 60のNoteOn/Offを1組、denseはkey 60〜67の16組を均等配置する。別の新規Engineで64ブロックを検証し、全スケジュール行・終端行のfinite検査とchecksumを取る。最終前後の90条件すべてで一致した。

固定CPUで移行前→初期候補→最終候補の順に各1回実行したうち、移行前と最終候補を保存する。Audio / Noteのような3組比較ではない。[pinned-before.txt](data/keyparam/pinned-before.txt)と[pinned-final.txt](data/keyparam/pinned-final.txt)にはcompile統計も含む。[comparison.csv](data/keyparam/comparison.csv)は旧比較表から中間候補の列だけを除いたもの。`change_pct`は前後中央値の増減率、`*_spread`はp90 / p10。

### 結果と採用した調整

512 frames、periodic、単位µs/block。全条件はCSVを参照。

| fixture | resolution 32: 前 → 後 | 増減 | resolution 1: 前 → 後 | 増減 |
| --- | ---: | ---: | ---: | ---: |
| core | 1.89 → 1.85 | −2% | 51.39 → 49.35 | −4% |
| key2 | 1.69 → 1.86 | ＋10% | 44.54 → 48.12 | ＋8% |
| key8 | 4.27 → 2.56 | −40% | 124.49 → 64.15 | −48% |
| mixed | 3.58 → 2.27 | −37% | 101.34 → 60.74 | −40% |
| key8_muted | 4.39 → 2.52 | −43% | 124.35 → 66.14 | −47% |

2候補は18条件で約4〜22%増加し、periodicの6条件では約8〜16%増加した。上表のresolution 32では12ノード合計で約0.17µs/block増える。Custom呼び出し・Context構築・候補位置への変換等の固定費が残る。一方、8候補は細かいKeyLatch / Latch / Select列をまとめる効果が大きく、18条件で約32〜50%、混在構成では約21〜41%短縮した。

`compile`と生成Programの破棄は16回ずつの平均を21回計測した。中央値はkey2が32.26→30.91µs、key8が45.52→42.50µs、mixedが43.55→40.79µs。Graph構築・prepare / publish / adoptは含まない。

Customをenum末尾へ置き、既存variantの順序を保った。初期候補にあったCoreの数%の低下が抑えられたため採用したが、生成コードへの効果はコンパイラ・CPUに依存する。入力・NoteビューとContext構築は既にインライン展開されており、追加のinline指定は行っていない。境界検査やf64ラッチ変換を避けるunsafe化、候補数で旧IRとCustomを切り替える二重実装は採らなかった。

## Audio / Note

### 構成と手法

[hybrid_audio_note.rs](../../../crates/audio-graph-engine/examples/hybrid_audio_note.rs)で同じGraphの前後を比較する。疑似Pluginは入力イベントを記録して無音を出す。計時外で初期状態から64ブロックを実行し、全音声のfinite検査、サンプルの全ビット・受信イベント・終了通知のchecksumを取る。

| 系統 | Graph | 条件数 |
| --- | --- | ---: |
| Audio Core | 固定ゲインMix × 6を直列 | 12 |
| Audio chain | AudioTremolo × 6を直列、消費後の入力領域を再利用 | 16 |
| Audio fanout | 同じ入力からAudioTremolo × 6へ分岐してMix、未消費の読者がいる枝は別出力領域 | 16 |
| Note route | 同じNoteInをNoteMergeの6入力へ接続、重複接続を簡約 | 6 |
| Note filter | NoteFilter × 6へ分岐してNoteMerge | 6 |
| Note delay | MidiDelay × 6へ分岐してNoteMerge | 6 |

Audioはstereo、64 / 512 frames、whole / quantum 32 / quantum 16、idle / periodic。chain / fanoutにはmonoのwhole条件も加える。resolutionは32。quantum条件は0.01秒のDelayRead / Writeと−18 dBの帰還Mixを追加した実際の回路で、SubBlockになったことをassertする。wholeとの絶対時間差には追加回路の仕事も含まれる。

AudioTremoloはSelect、周期1/64 beat、sound:cut = 1:1、fade in/out各0.25 ms。periodicは16ブロックごとに再打鍵し、64 framesでも切断・再開の境界を越える。idleは未起動。入力は非ゼロの有限な周期信号。

Noteは512 frames、resolution 32 / 1、idle / periodic / dense。periodicは毎ブロック1組、denseは16組のNoteOn/Off。MidiDelayは144〜149 samplesの遅延で、ブロックをまたぐ保留イベントを含む。NoteFilterはchannel 0を通しCC 1を拒否するが、この入力のNoteOn/Offはすべて通る。Mergeは同音の分岐を重複排除し、Delay各枝は別時刻に出力する。起動直後のDelayのイベント数を他構成へ揃える補正はしない。

反復数は16の倍数。固定CPUで前→後、後→前、前→後の3組を実行した。62条件すべてで各組のchecksumとイベント数が一致した。[comparison.csv](data/audio-note/comparison.csv)は各実行の中央値3個の中央値と、対応する3組の増減率の最小・最大を保存する。生データは同じディレクトリの`before-1.csv`〜`before-3.csv`、`after-1.csv`〜`after-3.csv`。

### 結果と採用した調整

| 条件 | 前 µs/block | 後 µs/block | 増減 |
| --- | ---: | ---: | ---: |
| Audio chain / stereo / 512 / whole / idle | 9.281 | 1.082 | −88.3% |
| Audio chain / stereo / 512 / whole / periodic | 18.025 | 17.946 | −0.4% |
| Audio fanout / mono / 64 / whole / periodic | 2.593 | 2.730 | ＋5.3%、＋0.137µs |
| Audio fanout / stereo / 512 / quantum 32 / periodic | 34.818 | 35.773 | ＋2.7%、＋0.955µs |
| MidiDelay / resolution 32 / idle | 1.222 | 1.105 | −9.6% |
| MidiDelay / resolution 32 / periodic | 1.721 | 1.787 | ＋3.8%、＋0.066µs |
| MidiDelay / resolution 32 / dense | 7.855 | 8.142 | ＋3.7%、＋0.287µs |
| MidiDelay / resolution 1 / idle | 28.546 | 25.202 | −11.7% |
| MidiDelay / resolution 1 / periodic | 29.992 | 27.990 | −6.7% |
| MidiDelay / resolution 1 / dense | 44.563 | 36.845 | −17.3% |

Audio停止中はchainが25.1〜91.3%、fanoutが19.3〜75.6%短縮。動作中はchainが−0.4〜＋2.4%、fanoutが＋1.6〜＋5.3%で、後者は全条件で増加した。割合の最大増分はmono / 64、絶対時間の最大増分はstereo / 512 / quantum 32。いずれも上表に示した。

Customを通らない基準構成もAudio Coreで−3.3〜＋3.3%（最大＋0.372µs）、NoteFilterで−1.8〜＋2.7%（最大＋0.137µs）、Note routeで−5.6〜＋1.3%（最大＋0.174µs）の差がある。

これは**Custom化とノード内の改善を合わせた製品変更**の結果。Audioの停止中・gain 1・入力イベントなしの通過処理、Noteの絶対期限キューと無入力時の省略、行情報の共有、小さい借用補助関数のinline化を含む。dyn dispatch単体がOpより速いとは結論しない。`AudioOp`は120→64 bytes、`NoteOp`は96 bytes、Paramの`Op`は48 bytesを維持したが、別途所有するBox・状態・bindingsを含む総メモリ量ではない。

### 採用時間

最終実装のAudioTremoloを16 / 64 / 256個直列にしたGraphで、`Engine::adopt`だけを別途測った。変更前との比較ではない。48 kHz / 512 framesでprepareし、16回の予備実行後に101回計測。compile / publish / reclaimは計時外、音声評価は挟まない。逆順条件は同じNodeIdの接続順を反転して交互にpublishする。

| Custom数 | 同順序 median / p10 / p90 µs | 逆順序 median / p10 / p90 µs |
| --- | ---: | ---: |
| 16 | 0.4 / 0.4 / 0.5 | 0.4 / 0.4 / 0.5 |
| 64 | 1.5 / 1.4 / 1.5 | 1.5 / 1.4 / 1.5 |
| 256 | 10.5 / 10.4 / 11.1 | 10.6 / 10.4 / 12.8 |

[adoption.csv](data/audio-note/adoption.csv)はこの集計値を保存する。照合は旧配列の線形探索を繰り返すO(n²)であり、逆順でも照合位置の合計がほぼ同じになる。さらに大きいGraphや別の状態内容での上限は保証しない。**一時測定プログラムはGitに保存していないため、この測定は2つのexampleからは再現できない。** 再検証が必要なら上記の条件で計測器を用意する。

## 検証で守った挙動

性能比較のchecksum一致だけでは、編集時の状態移行や容量境界は証明できない。製品側の回帰テスト・統合テストで次を確認した。

- KeyParamのSelect優先順位、重複・無効キー、Toggleの行単位更新、値不足の0、候補縮小時の生ラッチ・未設定NaNの保持、終端先読みからの復元。未接続・空キーの簡約と、複数出力・飛び飛びの入力を借用する契約。
- 1候補の配線値は出力aliasを維持する。別stageの上流LFOの行値`[0, 0.25, 0.5, 0.75]`が最後の値へ潰れる回帰を検出し、状態更新用Customを残しながら出力を上流レジスタへbindして修正した。この条件は90条件の計測fixtureには含まれない。
- Audioのin-place / separate、入力領域の前後両配置、strideの隙間、隣接領域、0チャネル、端数chunk、左右で異なる信号、fade中の再採用。入力切断中はSilenceだけを実行し、クロックとgainは**進まず保持される**。
- MidiDelayの可変block / row長、tempo変更、FIFO、NoteOffが対応NoteOnを追い越さないこと、キュー容量と破棄、ノード削除時の所有権返却。配線された遅延値を行ごとに読み、負数を0へクランプすること。
- 同じ型のruntime各2個を並べ替えても、状態がNodeIdに追随し新設定を使うこと。NodeId・具体型の不一致では初期状態を使うこと。NoteFollowのGate / HeldKeysが次行へ反映され、stream番号変更後も保持・解除されること。
- 採用・評価・reset・除去等での確保0・解放0、退役資源の非RT側破棄、ノート所有権の二重解放防止。Wrapperのresetでは保留ノートを捨て、終了通知を1回だけ返すこと。

2026-10-19の追加レビューでは`temp/astra_handout_report.md`のPart 1に対応する7テストを追加し、製品コード変更は不要だった。既存テストにもNodeId / 型の不一致検証があったこと、切断中のクロックは進まないこと、直接配置テストの欠如とコード未通過の証明は異なることを訂正した。Part 2の後続実装は未検証・未着手であり、採用済みの計画としては扱わない。

`6a0be82`時点の結果は`cargo test -p audio-graph-engine`が244件、`--features ui`付きが256件、`cargo test -p audio-graph-plugin`が81件成功、`cargo fmt --all -- --check`も成功。件数は統合テスト・doc testを含む。plugin全体は先に`cargo build -p clap-test-plugin`で既存fixtureのライブラリを用意して再実行した。移行時にはworkspace全体のcheckも成功している。これらは当時の結果であり、文書整理時の再実行結果ではない。

## 既知の挙動と範囲

同じWrapperへプリセットを再読込すると、NodeIdが一致するKeyParamの選択状態が残る。「reloadでは最初の候補へ戻る」という説明と一致しないが、移行前にも再現したためリセット方針を変えていない。再現はSelect、keys `[60,61]`、values `[-12,-6]`で61を打鍵して保存し、同じWrapperでvaluesを`[-24,-18]`へ編集・公開・処理した後、保存状態を復元してactivate・処理する。出力は`-12`でなく`-6`になる。新規Wrapperへの復元テストとは区別する。

AudioTremoloはMIDI Tremoloと共有していた固定状態poolを使わず、Audio側には合算16個の制限を適用しない。MIDI側のpoolは維持する。MidiDelayのキュー容量256と、Note buffer全体の上限16は維持する。KeyParamは中間レジスタ削減により以前は容量不足だったGraphが通る場合があるが、上限値自体は変えていない。

今後の拡張条件は[ADRの再検討条件](README.md#再検討する条件)を参照する。全面dyn化、KeySwitch等の移行、別件の性能改善はこの整理に含めない。

## 再現とデータの来歴

現在のexampleを動かす場合はリポジトリルートから以下を実行する。出力先はGit管理外の`target`にし、保存済みの測定データを上書きしない。2つの実行やビルド・テストを同時に走らせない。

```powershell
New-Item -ItemType Directory -Force target/hybrid-repro | Out-Null
cargo build -p audio-graph-engine --release --example hybrid_keyparam
cargo build -p audio-graph-engine --release --example hybrid_audio_note
cmd /c 'start "" /b /wait /affinity 4 target\release\examples\hybrid_keyparam.exe > target\hybrid-repro\keyparam.txt'
cmd /c 'start "" /b /wait /affinity 4 target\release\examples\hybrid_audio_note.exe > target\hybrid-repro\audio-note.csv'
```

当時の前後比較を再構築する場合は、上の履歴表のコミットを別のdetached worktreeへ展開してビルドする。KeyParamは`4f9f020`と`f732cbd`が同じ計測ソースを持つ。Audio / Noteは`f732cbd`と`de0e9c2`を使い、前者にはexampleがないため、後者の`hybrid_audio_note.rs`をそのままコピーして両側で使用する。同じソース・環境・CPU固定・実行順で比較し、タイミング値そのものの一致は期待しない。

Audio / Noteの計測後、`81f83d1`でexampleの`% 16 != 0`を`!is_multiple_of(16)`へ変更した。現在のexampleは再利用できるが、保存データのソースを特定する場合は`de0e9c2`を使う。以下のSHA-256は測定時の識別情報であり、バイナリの存在や再ビルド時の同一ハッシュを保証するものではない。バイナリはGitに保存していない。

| 対象 | SHA-256 |
| --- | --- |
| Audio / Note移行前の実行ファイル | `279C4ED51287206329F1E742881140A327F26A3E4C6AD073B48180C17526B6A9` |
| Audio / Note移行後の実行ファイル | `BAA3ECBC3757E4417C8E0630AD4887D1A1EF0D0E8D363B1B7ADAED42EB64362B` |
| 採用時間の一時実行ファイル | `4DEE2064D2FEDF0A9C5739C7FE311C0A9D17F621EB59F853FD4030E4F133AB4E` |
| `de0e9c2`のAudio / Note example（LF・末尾改行あり） | `484EC07D40DAD613E662A6C2826B0BE61338B7AB682663573709B2B93070EBE5` |

文書整理ではKeyParamの前後生ログ2本、Audio / Noteの生データ6本・比較表・採用集計を内容を変えずに移動した。KeyParam比較表は中間候補の`initial_ns`・`initial_to_final_pct`を除き、残る値を維持した。CPU未固定の試行、中間候補、重複した計画・報告は、この作業ブランチの履歴から除いた。ここに保存した最終資料を判断根拠とし、中間ログは保存対象にしない。
