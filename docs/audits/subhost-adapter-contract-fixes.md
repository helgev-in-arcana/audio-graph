# subhost-adapterの契約修正

2026-09-13。基点`9bff97b`、branch `refacter/enforce-subhost-adapter-contracts`。
監査のA1〜A5と、別枠だったsub-block transportの原点補正を実装した。

## 結果と変更範囲

| 項目 | 修正後の動作 | 主なcommit |
| --- | --- | --- |
| A1 永続instance | ロード不能・native state復元失敗でもreference/blobを保持する。欠落枠は空き枠にしない。明示unloadで初めて保存情報を削除し、native保存失敗では最後のblobを維持する | `0650a44` |
| A2 発生元 | main通知と合流後のaudio出力にinstance indexとロード世代を保持する。古いProcessorの出力を新しい子と区別できる | `da72803` |
| A3 入力競合 | 準備時にparameterごとに採用laneを選び、選ばれた実効値を重複排除する。動いた低優先laneだけが送信されて上書きすることを防ぐ | `1842a29` |
| A4 寸法 | activate前にlane/instance/I/O/容量を検査する。scheduleの構築・begin・view構築は不整合を拒否する。処理時はchunkとscheduleの整合を検査する | `2cca4de` |
| A5 探索対象 | state復元へcallerの探索folderを渡す。製品はbrowserと同じユーザー設定を使う | `36b1ddd`、`da94095` |
| transport | parent blockのtransportをchunk開始へ進める。停止中は進めず、既知のloop境界と小節開始位置も補正する | `190d798` |

さらに最終レビューで、quantum変更時に既存blockの行数も更新する処理と、native出力へのsource付与が無割当であることを確認するテストを補った。

adapterの新規moduleは`context.rs`と`events.rs`。一個のpluginを扱うplugin-host-api/plugin-host/VST3/CLAP backendの公開APIと実装は変更していない。CLAP test fixtureの追加機能は試験用であり、出荷するbackendの機能追加ではない。新しいクレートや依存ライブラリーは追加していない。

## コード量

基点との差は、テストを除くRustコードで**追加482行・削除136行、純増346行**。sourceのコメント・空行を含み、integration test、cfg(test)配下、fixture、doc code、文書、manifest/lockfileを除く。transportの補正もこの数に含む。

| 工程 | 製品コード純増 |
| --- | ---: |
| A5と型追従 | +6 |
| A1 | +17 |
| A3 | +33 |
| A4 | +92 |
| A2 | +159 |
| transport | +35 |
| 最終検証・doc追従 | +4 |
| 合計 | **+346** |

一番大きいA2は、発生元を残すmain通知契約とnative向けwrapper、有界のtagged outputを追加するため。A1は既存のInstanceStateとnative sparse tableを再利用し、保存・復元処理を置き換えたことで純増を抑えた。旧native表の各Loadedは派生情報を持ち、文書の正本は別の保存entry集合に残す。

工程ごとに同じfileを編集するため、追加・削除の工程別合計は最終diffと一致しない。旧監査の失敗再現probeは今回の回帰testにはせず、期待値を修正後の契約にした正式testを追加した。

## 公開APIの移行

| API | 移行方法 |
| --- | --- |
| `SubHost::new` | `Arc<dyn SubHostContext>`を渡す。main通知の各methodで`InstanceId`を受け取る。native backendにはadapter内部のwrapperが従来のHostContextとして渡る |
| `SubHostConfig` | `target_priority`を指定する。`PreferSlots`、`PreferDirect`、`RejectConflicts`から選ぶ。同種の競合は後ろのlaneが勝つ。AudioGraph製品とCLIはPreferDirect |
| `SubHost::load_state` | `search_directories: &[(Format, PathBuf)]`を追加。製品は設定のPathBuf一覧を既存plugin_directoriesで形式付きへ変換する |
| `SubHost::reference` | 未ロードの保存entryでもSomeを返す。nativeの有無はis_loadedで判断する |
| `SubHost::unload/unload_all` | 明示的に保存entryも削除する。失敗した復元の一時解除はadapter内部で区別する |
| `SubHost::source` / `SubHostProcessor::source` | 現在の子、またはProcessorが保持する子のInstanceIdを取得する。世代は実行時の識別用で保存しない |
| `SubHostProcessors::bind` | `InstanceEventSink`を渡す。events()の各要素は`{ source, event }`。overflowは子・chunkを跨いで保持する |
| `SubHostProcessor::process` | 引数形は維持。直接一個の子を処理する場合は従来EventSinkを使い、必要ならsource()と組にする。contextはparent block先頭の情報を渡す |
| `SlotSchedule::new/begin` | `Result<_, &'static str>`を処理する。beginの上限超過では部分scheduleを作らず、frames/blocksを0へ戻す |
| `SlotSchedule::max_frames` | 準備したframe上限を取得する。製品は超過blockをbuffer操作前に消音Errorへする |
| `ScheduleView::from_parts` | `Result<ScheduleView, &'static str>`を処理する。row×lane数、frame数とquantumを検査する |
| `Engine::end_block` | `IntoIterator<Item=&Event>`を受ける。製品はtagged outputからeventへの投影を渡し、一時Vecを確保しない。既存のEvent sliceも引き続き渡せる |

`set_quantum`は事前確保した範囲内で行数を更新する。呼び出し側は新しいgridの値を書いてからprocessする。既存の値を新しい時刻へ自動補間するAPIではない。

## 永続情報と失敗の扱い

保存entryはnative instanceとは別に保持する。未対応format、欠落module、native stateの拒否でも、元のreferenceとencoded blobが残る。現在のinstance上限を超えたentryも保持するが、そのindexまでnative sparse tableを拡張しない。

native復元が失敗した子は処理へ採用せず、元の保存entryを保持する。default presetのnative instanceを保存して元blobを失うことを防ぐ。正常ロード後のnative save失敗では最後に保存できたblobを使い、その後のsaveが成功した時点で更新する。ユーザーの明示削除や置換は保存情報も更新する。

製品の保存形式・versionは維持した。欠落した子の名前は保存済みreferenceから表示し、nativeが存在しないことをloaded=falseで表す。専用の復旧wizardは追加していない。

## AudioGraphからの独立性と実行時コスト

- 優先順位はSubHostConfigでcallerが選ぶ。adapterにGraph/Node/Stage/Programの知識は追加していない。
- sourceは同じadapter実装内で新しくロードした子ごとに発行する。別SubHostの生成でカウンターを巻き戻さない。世代付きの寿命管理registryを増やすものではない。
- source付き出力は、事前確保したnative scratchからtagged storageへ一回コピーする。audio側に確保・待機を追加しない。main→audioの既存try_lockはbackendに残る。
- parameterの優先順位決定はactivate時。processでは選ばれたtargetだけを読み、前回の実効値と比較する。
- transportはactivationのsample rateとblock先頭のtempo/meterを使う。停止時は位置を固定する。loop位置は提供されている単位の有効な境界を使う。block内部のtempo/meter変化や、未提供のloop時間変換を新しく推定する機能は追加していない。
- graph全体のlatency補償やnoteの配送先は従来どおり上位の責務。scannerのprocess隔離も今回の範囲外。

## 検証

- Windows: adapter unit32件、native integration9件、engine（ui有効）179件、AudioGraph製品52件、CLAP backendの既存fixture test14件成功。
- 製品のeditor_actionsは同梱CLAPを`AUDIO_GRAPH_TEST_SUB`に指定し、製品一括実行は1 test threadで検証。任意のinstalled pluginを検証対象にしていない。
- Linux: 既存WSL Ubuntu-24.04でadapter unit32件、native integration9件、CLAP backendの既存fixture test15件成功。adapterのall-targets Clippyも-D warningsで成功。
- source付きnative出力の収集で、ホスト側allocation/deallocationが0/0であることを実測。第三者DLL内部のallocatorを測定したものではない。
- 形式・静的検査: cargo fmt、git diff --check、関連クレートのall-targets Clippy。Windowsでは既存fixture由来のdefault_constructed_unit_structs lintを許可し、その他は-D warnings。
- macOS/Windows ARMのローカル実行、実DAWのGUI操作、CPU時間のベンチマークは未実施。

新規native testは、解決不能entryの再保存、上限外entry、native save/loadの失敗、caller指定folderからの復元、複数laneの優先順位、準備不整合の拒否後のretry、同一ParamIdを持つ複数子、置換後の旧Processor出力、chunk transportを検証する。

寸法検査の追加で、既存testの4frame buffer対128frame scheduleと、1row対2row分のquantum指定の不整合も検出した。前者はbufferをscheduleのframe数に合わせ、後者は検証目的どおり1rowのquantumに合わせた。拒否条件を緩めてtestを通す変更はしていない。
