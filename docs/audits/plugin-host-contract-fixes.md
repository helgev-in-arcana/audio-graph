# plugin-host契約の修正と検証

更新: 2026-09-12。監査基準 `e2b629b`。実装完了点 `03cb68e`。対象は[監査F1〜F5](plugin-host-review-2026-09-11.md)。

## 変更とレビュー順

| 工程 | 変更 | コミット | 実装純増 |
| --- | --- | --- | ---: |
| F1 | checkedなサイズ検証と、両backendのprocess入口で構成一致を強制 | `0c5a963` | +51 |
| F2 | EventSink容量を固定。native/共通出力の超過を保持し製品で回復 | `1b537af` | +94 |
| F5 | native NoteEndと上位回収方針を分離。既存台帳に配送先別記録 | `c9c8518`、未ロード時の追従`632350f` | +101 |
| F3 | native要求を記録しmainのtickで配送。再要求を保持 | `2904f76` | +29 |
| F4 | 明示的metadata更新、純粋なgetter、callerの構成選択、停止・再公開・復旧 | `27655eb`、state復元順序の追従`03cb68e` | +354 |
| 合計 | テスト・fixture・文書を除く追加−削除 | | **+629** |

コメント・空行を含む物理行数。src内の`#[cfg(test)]`もテストへ分離した。テスト・fixtureの純増は1,061行。文書は別コミットで扱う。行数を縮めるための式の圧縮や新しい汎用管理器は導入していない。

F1はAudioConfig::validateとAudioBuffers::matches_configへ検証を寄せた。safeな呼出しでchannel/aux境界/配置が一致しない場合、native pointerを作る前にErrorと音声消去を返す。0-frameはnativeへ送らない。

F2はcallerが収集区間のclearを所有する。backendは追記し、複数instance/chunkで先行出力と超過を消さない。adapterが追加分の時刻をblock先頭基準へ直す。容量超過時は、製品がprocessorと台帳をresetし親hostへvoice終了を返す。CLIは不完全なrenderを成功扱いしない。

F5はCLAP入力portの対応方言からnote_end_portsを取得し、activate時にadapterへ保持する。VST3の入力NoteOffからの合成終了を削除し、native出力NoteOffも意味を変えない。AudioInstances経由の能力問い合わせでengineが回収を選ぶ。ゲートでonを受けずoffだけ受けた枝が他の声を減らす問題を再現したため、ユーザー承認の上で既存NoteLedgerを拡張した。配送先、port、noteのserialと回数を照合し、プログラム更新では記録を引き継ぐ。別のvoice managerは作っていない。

F3はCLAPの既存要求フラグを再利用し、VST3にもcoalesce用のatomic flagsを置いた。HostContextへのrestart通知はmain保守時に配送する。VST3のGUI parameter通知とactivation時のlatency通知はmainから同期配送する。受信側は同じpluginへ同期再入せず処理を予定する。通常終了では最後にtickし、destroy時の未配送要求は破棄する。

F4のrefresh_metadataはUnchanged/Refreshed/NeedsDeactivationを返す。表示だけの変更は処理を継続し、parameter集合・範囲等やI/Oの変更は停止してから適用する。読取失敗・読取中の再要求は更新完了にせず、未完了中のactivateも拒否する。音声ポートとslot対応を更新してから既存のShared::publishへ進む。途中で失敗した場合は停止状態を維持し、次の更新・再公開で回復できる。再構成で停止した声は次の実行ブロックで親hostへ返す。保存復元では先行callbackを処理してからstateを適用する。

## 実行時コスト

| 経路 | 追加される処理 |
| --- | --- |
| 通常の音声block | 構成の固定個数比較、出力の容量確認、再構成後のノート整理要否の確認。これらによるheap確保はない |
| note on/off | fallback側で配送先を検索し、既存台帳の固定領域のcounterを更新。検索はinstance数に比例 |
| graph準備・採用 | 配送記録をmainで確保し、採用時に同じinstanceの記録をコピー。約4KiB/instance（256note、現行64-bit layout）。古い領域はmainへ返却 |
| native変更要求 | atomic flagの記録。mainでまとめて配送 |
| metadata変更 | mainで既存readerを使い再取得・比較。構造変更時だけ停止・再有効化・graph再公開。列挙用の確保もmain側 |
| 出力超過 | 例外経路としてprocessor/台帳resetと親hostへの終了通知 |

通常の音声経路全体から既存mutex操作を除去した変更ではない。main→audio編集queueのtry_lockは残る。第三者plugin内部の確保や待機も、この検証の保証外。

## 検証

- 共通API unit 19件と、出力超過を含むProcessorのallocator試験。
- CLAP backend unit 17件、同梱native fixture integration 11件。構成不一致がnative編集を適用しないこと、容量0/超過、native NoteEnd、音声threadからの要求、callback中の再要求、metadata変更・失敗・再試行を確認。
- VST3 backend unit 33件。native出力容量、要求のcoalesce、NoteOffの意味保持を確認。
- plugin-host unit 2件、catalogue 1件、CLAP facade 1件。subhost-adapter unit 29件。
- engineはUI featureを含むunit 162件、integration 17件、compile-fail doc 3件。混在・ゲート・台帳引継ぎ・ID再利用を含む。未ロード側追従後に通常featureのunit 157件も確認。
- 製品unit/integration 52件。audio_path、audio_thread、editor_actions、host_lifecycle、latency、reactivation、reset、state_reloadを同梱`.clap`で実行。スキップなし。metadata読取失敗からの復旧と動的state復元は追加後に対象testを再実行。
- 関連crateのall-targets Clippyを実行。既存Windows fixtureの`clippy::default_constructed_unit_structs`だけをコマンドで除外。新規lintは残していない。format、diff、API docを最終確認。

実DAW、インストール済みVST3のnative統合実行、Windows以外の実行、Miri/ASanは未実施。VST3のmetadata交渉・更新はコード確認とコンパイルが中心で、CLAPと同等のnative fixture検証を行ったという意味ではない。

元の監査報告は基準版の記録として保持する。scanner隔離、追加の共通契約crate分離、未対応規格イベントや局所的な変換問題は今回の修正へ混ぜていない。
