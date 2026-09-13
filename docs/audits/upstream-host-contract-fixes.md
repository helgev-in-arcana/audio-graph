# 上流native host契約の修正

2026-09-12。基点は`fc0d647`、実装ブランチは`refacter/close-native-host-contracts`。
対象はplugin-hostより上流の監査で挙げたU1〜U9。規格差の吸収とnative資源の管理はbackendに置き、処理失敗への製品側の回復だけをadapter・AudioGraphへ接続した。

## 変更単位とコード量

純増はRustの物理行数で、コメント・空行を含む。integration test、`cfg(test)`配下、docのコード例、両test-pluginクレート、文書、manifest、lockfileを除外する。基点との差は追加705行・削除356行、**純増349行**。工程間の編集の重なりがあるため、追加・削除それぞれの工程別合計は最終差分と一致しない。

| ID | 完了した変更 | 主な実装ファイル（cratesからの相対path） | commit | 純増 |
| --- | --- | --- | --- | ---: |
| U1 | binaryを一つの所有threadへ限定。同じthreadでは共有し、別threadからのopenはbusy。catalogueはbusyを恒久的な故障にしない | `clap-host/src/module.rs`、`vst3-host/src/module.rs`、両`library.rs`、`plugin-host-api/src/lib.rs`、`plugin-host/src/catalogue.rs` | `7f98135` | +63 |
| U2 | Win32 classをWndProcの所在moduleで登録し、最後のwindowの破棄後に登録を解放 | `host-window/src/win32/window.rs` | `f3c4e9f` | +45 |
| U3 | instanceとmoduleを保持する所有viewをeditorまで引き渡す | `vst3-host/src/plugin.rs`、`lib.rs`、`vst3-host-view/src/editor.rs` | `df757c8` | +18 |
| U4 | frame/run loopの不要なunsafe Send/Syncを除去 | `vst3-host-view/src/frame.rs` | `7da3a19` | −8 |
| U5 | VST3のGUI→DSP、DSP→controller、native parameter出力を接続。CLAPのflush要求とmain通知を完了 | VST3の`host_app.rs`、`plugin.rs`、`param_sync.rs`、`param_map.rs`、`process_io.rs`。CLAPの`host.rs`、`plugin.rs`、`events.rs`。共通`traits.rs` | `1773e80` | +152 |
| U6 | 不完全な入力batchをnative呼出し前に拒否。main編集を拒否時に保持し、製品の既存reset経路へ処理失敗を伝達 | 両backendの`plugin.rs`、CLAPの`events.rs`、共通`traits.rs`、`subhost-adapter/src/host.rs`、`audio-graph-plugin/src/plugin.rs`、`host-cli/src/main.rs` | `a0616b6` | +62 |
| U7 | 要求したbusと実arrangement・channel countを検証し、未対応構成を拒否 | `vst3-host/src/plugin.rs`、`plugin-host-api/src/traits.rs` | `7a8904a` | −21 |
| U8 | COM初期化の成功と解除をResult＋非Send guardで表し、CLI/scanner等が保持 | `vst3-host/src/com.rs`、`lib.rs`、`plugin-host/src/lib.rs`、`host-cli/src/main.rs`、`audio-graph-plugin/src/editor.rs` | `a4de172` | +17 |
| U9 | watcherの登録世代を照合。Linuxのnative handlerをcallback中だけ追加保持 | `host-window/src/watch.rs`、`vst3-host-view/src/frame.rs` | `f74d2a4` | +21 |

新しい製品用クレートは追加していない。U5のDSP→main配送だけを32行のprivate module `param_sync.rs`へ分離した。既存のmain→audio queue、parameter変換表、Error、note reset処理を再利用した。

差分全体には、別component/controller・view・native callbackを持つ`vst3-test-plugin`も含む。これは公開・出荷しない検証用cdylibで、vst3 0.3.0のgain exampleを基にし、元のMIT/Apacheライセンスを同梱している。CIの既存workspace buildでCLAP/VST3両fixtureを構築する。

## 公開APIの追従

| 利用面 | 変更と移行方法 |
| --- | --- |
| `plugin_host::init_thread()` | `Result<ThreadGuard>`を返す。`let _thread = plugin_host::init_thread()?;`として全native資源の解放・processor返却完了まで保持する |
| `vst3_host::init_apartment()` | `Result<ApartmentGuard>`を返す。同じ保持条件。MTAの既存threadではエラーとなる |
| `HostError` | `ModuleBusy(String)`追加。網羅的matchは追従する。別所有threadが同じbinaryを使用している間の一時的な拒否 |
| `Vst3Plugin::create_view()` | `Option<Vst3View>`を返す。`EditorWindow::open`と`can_resize`もこの型を使う。raw pointerは借用で、そこから得たnative参照もhandleより先に解放する |
| `PlugFrame` | Send/Syncではない。UI thread内で生成・使用・破棄する |
| `SubHostProcessors::failed()` | block内の処理失敗を照会する追加API。bind時に新しいblockとして初期化し、後続処理の成功では失敗を消さない |
| `tick` / `snapshot` / `set_param` / `param_edited` | シグネチャ維持。nativeの値同期はbackendが担当し、main通知はplain値。snapshot前にtickを進める |
| `process` / `activate` | シグネチャ維持。容量・入力時刻違反はError、nativeの実busが一致しない構成はactivate失敗として扱う |
| facadeのeditor操作、window、watcher | 公開シグネチャ維持。watcherの世代番号は内部に隠蔽する |

`vst3-host-view → vst3-host`の依存を追加した。viewを使う層が、その寿命を保証するbackendの所有handleへ依存する。`plugin-host-api`へVST3の型やGUI寿命管理を持ち込んでいない。

## 値配送と実行時の仕事

VST3ではGUIと呼び出し側の編集を同じnormalized queueへ入れ、そのblockのautomationより前に配送する。処理済み入力とnative出力の最新値をparameter別AtomicU64へ保存し、mainのtickがcontrollerへ反映する。main側で次の配送を待つ編集は、先行blockのfeedbackで上書きしない。表示向けの途中値は合流できるが、processへ渡すsample単位のイベントを表示queueへ置き換えるものではない。native出力はplain値へ変換してEventSinkにも渡す。state復元時は古い編集とfeedbackを除く。

CLAPではrequest_flushをon_main_thread要求から独立させた。inactiveならhost入力がなくてもflushし、出力値をmainへ通知する。activeなら次のprocessで交換し、audioと同時にnative flushを呼ばない。古いhost編集を再activate時に再送して新しいGUI値を消すことも防ぐ。

| 経路 | 追加・変更された仕事 |
| --- | --- |
| module open/drop | binaryの正規化と所有者表のmutex操作。音声blockでは行わない |
| window/view作成・破棄 | class利用数の管理、viewのinstance参照保持。音声blockでは行わない |
| activate | 実busの検証、parameter feedback領域の準備 |
| 音声block | 入力の容量・順序・時刻検査。VST3は変更parameterの探索とatomic store、native出力値の変換・配送。adapterは失敗を集約 |
| main tick | feedbackのatomic読み取りとcontroller更新、GUI通知、CLAPのinactive flush |
| watcher dispatch | 登録世代の照合。VST3/Linuxはcallback一回につき一時AddRef/Release |
| thread初期化・終了 | 成功ごとにOleInitialize/OleUninitializeを対応付ける |

音声経路に新しい待機mutexや動的確保は追加していない。main→audio queueの既存try_lockは残る。CLAP入力のsortを除去し、呼び出し側が順序を守る共通契約を検査する。CPU時間のベンチマークは今回実施していない。

## 検証

| 環境・範囲 | 結果 |
| --- | --- |
| Windows、上流host＋adapterのunit | 117件成功 |
| Windows、engine unit/integration | 179件成功 |
| Windows、AudioGraph製品unit/integration | 52件成功。editor_actionsには同梱CLAP fixtureを明示し、この製品一括実行は1 test threadで実行 |
| Windows、VST3/CLAP native契約・catalogue・processor allocation・window class | 23件成功 |
| Windows、frame/guard compile-fail doc | 3件成功 |
| WSL Ubuntu-24.04、window unit | 21件成功。fd再登録と実X11 windowを含む |
| WSL、共通API/VST3/view unit、VST3/CLAP native契約、catalogue | 80件成功 |
| format・差分 | `cargo fmt --all`、`git diff --check`成功 |
| Clippy | 変更関連クレートの`--all-targets -D warnings`成功。ただし既存のWindows CLAP fixture由来`default_constructed_unit_structs`を許可 |

native回帰テストはmoduleの重複openと解放後の再open、view保持中のmodule寿命、別audio threadでのGUI/automation/出力値の往復、scale=10のplain変換、拒否した入力batchとmain編集保持、bus拒否後の正常activate、CLAPのinactive/active flushを確認する。CLAPホスト側では最大2048件の入力処理に対してallocation/deallocationとも0を確認した。第三者DLL内部のallocatorを測定したものではない。

Win32 classは正式testで最後のwindow後の登録解除と再登録を確認し、作業用DLLのprobeでもWndProcを含むDLLのHMODULEで登録されることを確認した。最終検証ではeditor_actionsが任意のinstalled pluginを選んでbus不一致となったため、同梱fixtureを指定して再実行した。初期化guardを探索helperだけで保持していたテストも、資源を所有するtest本体へ保持範囲を修正した。

## 維持する境界と検証の限界

- ModuleBusyの管理表は、このbackend実装のbinary内で共有する。他のDAW実装や、別DLLに組み込まれたhost実装まで含むOS全体の排他機構ではない。同じbackendの別threadから同じmoduleを同時に使用する構成は拒否する。
- scannerのprocess分離・timeoutは追加していない。native crash/hangを同一process内で回復できるという保証はない。
- 初期化guardを全資源より長く保持することは呼び出し側の条件。load APIへの必須token追加や、DAWが提供するUI threadの所有までは行っていない。
- VST3のbus対応をsurroundへ拡張していない。mono/stereoの実配置一致を検証し、main未接続でauxだけある構成も拒否する。
- snapshotは全parameterの原子的な一時点観測ではない。全restart flagへの機能追加、gestureの完全保存、親DAWのautomation録音は対象外。
- macOS、実DAWでのGUI操作、任意の第三者VST3群、Miri/ASanは未検証。

APIの背景と規格対応は[plugin-host-api解説](../plugin-host-api-guide.md)と[plugin-host解説](../plugin-host-guide.md)を参照。
