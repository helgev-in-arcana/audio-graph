# エラー通知とエディター表示要求

この文書は保留ブランチの実装案を説明する。mainへの採用は保留中であり、nice-plugのforkは作成しない。
依存側の変更は[再適用可能なパッチ](../patches/nice-plug/README.md)として保存する。

AudioGraphが検出したエラーの内容は、現在の文書に所属する通知状態へ保持する。
表示要求はnice-plugを通してDAWへ送り、DAWが開いたAudioGraphの画面で内容を表示する。
エラー文をDAW独自のダイアログへ渡すAPIではない。

## nice-plugにAPIを追加した理由

使用中のnice-plug 0.3.0には、VST3/CLAPで定義されている「ホストへ自分のエディター表示を要求する」操作が公開されていない。
ネイティブのホストコールバックを所有するのはnice-plugの外側ラッパーであり、AudioGraphのPlugin実装や、内側の子プラグインを扱うplugin-hostにはその操作の入口がない。

既存AsyncExecutorが実行できるのはPlugin側のタスクである。主スレッドへタスクを届けても、そのタスクから使えるホスト表示APIが増えるわけではない。
同様に、既存のEditor/GuiContextはエディター実装・表示後の操作を扱うが、閉じたエディターをDAWに開かせる入口にはなっていない。

根本の不足を埋める追加がrequest_editor_openである。G2の発生時点にはActivateContextを、通常稼働中には画面の生成前から得られるAsyncExecutorを入口にする。
取消・状態確認のハンドルと必ず後送りするキューは、任意のタイミングで使えて文書変更にも安全な共通経路にするための追加であり、規格の関数を一度呼ぶだけなら必要なAPI量はより少ない。
エラー内容・原本保持・通知するかの判断はAudioGraphに残すため、nice-plugにAudioGraph固有の知識は追加しない。

## 責務とAPI

| 層 | 担当 |
| --- | --- |
| AudioGraphの`Notifications` | 発生元ごとの現在のメッセージ、重複抑制、要求の取消 |
| `Shared::report_error(document, source, message)` | 文書世代を確認して報告を受け付ける。非音声スレッド用 |
| `Shared::clear_error(document, source)` | 指定した文書・発生元のエラー状態を解消する |
| nice-plugの`ActivateContext::request_editor_open` | activate中の要求を保留し、ロック解放後にキューへ渡す |
| nice-plugの`AsyncExecutor::request_editor_open` | 画面の有無によらず、主スレッドへ表示要求をキューイングする |
| `EditorOpenRequest` / `EditorOpenHandle` | 送信側の所有、要求の取消、結果の確認 |
| VST3/CLAPラッパー | ホストの対応状況・接続を確認し、ネイティブAPIを呼ぶ |

内側の`plugin-host-api`、`plugin-host`、`subhost-adapter`に表示方針は置かない。
子プラグイン自身がSDKで自分のGUIを要求する場合の受信処理は、この経路とは別の機能である。

## 接続しているエラー

- `State`: 保存されたグラフを読み取れない場合。G2の原本保持と編集・保存の保留は既存の復元処理が決める。
- `Graph`: グラフのコンパイル、処理構成の準備、メタデータ更新での失敗。
- `Processing`: 既存の処理失敗フラグまたはイベントバッファーの不足を検出して処理をリセットした場合。

通常のエラー通知はグラフの保存を止めない。G2では、通知が拒否されても未解釈の原本を保持する。
すべての`Result`、ログ、パニックを自動で通知する機構ではなく、呼び出し元が通知対象を明示する。

## 実行タイミング

G2は`activate`中に判定し、その呼び出しで受け取ったcontextへ要求を渡す。
nice-plugはPluginのロックを解放してからcontextを破棄し、要求を主スレッドのキューへ載せる。
CLAPの初回activateとプリセット再ロードで、同じ破棄順序を守る。

通常のエラーは、エディターが閉じている間も動く既存tickから送信する。
音声処理はエラー時だけ`AtomicU64`に実行中の文書世代を記録し、tickが文面を作る。
正常な音声ブロックには通知用の割り当て・ロック・ホスト呼び出しを追加しない。
通知のために次の音声ブロックやactivateを待つ必要はない。

主スレッドから要求しても、ホストへの表示要求をその場で実行しない。
ネストしたイベント処理でPluginまたはtask executorが使用中なら要求を取り消し、AudioGraphは次のtickで再手配する。
ホストを呼ぶ際は、ホストインターフェースを保持する内部の借用も解放する。

## 結果と重複の扱い

`Pending`は未送信、`Dispatching`はホスト呼び出し開始後を示す。
`Accepted`はホストの受付であり、描画完了や前面表示の保証ではない。
`Unsupported`と`Rejected`では自動的に再要求せず、内容を手動表示時にも読めるよう保持する。

`Cancelled`は送信前の取消、キューの破棄や不足、再入中の配送などで生じる。
現在も有効な通知なら次のtickで再手配できる。文書を置き換えた場合は通知状態そのものを破棄し、
未送信の旧要求も取り消す。遅れて到着したworker・音声側の報告は、文書世代が違えば捨てる。

発生元ごとに一件のメッセージを保持し、同じ内容が続く間は新しい表示要求を作らない。
すでに表示要求が待機していれば、別のエラーもその要求で表示できる。
画面が開いている間は内容を更新する。閉じた直後に同じ通知で開き直さない。
処理失敗の通知は成功した再activateまたは文書の置き換えで解除する。

## 対応形式

| 構成 | 配送 |
| --- | --- |
| Windows / macOSの外側VST3 | OSの主スレッドキューから`IComponentHandler2::requestOpenEditor("editor")` |
| Windows / macOS / Linuxの外側CLAP | ホストの`request_callback`から`clap_host_gui::request_show` |
| Linuxの外側VST3 | 非対応として扱う。既存worker fallbackからUI専用APIを呼ばない |

VST3はホスト接続前の要求を保持し、接続後に送る。任意の`IComponentHandler2`がなければ非対応になる。
CLAPはinit前の要求を保留し、initで取得したGUI拡張を使う。
この機能によってLinux外側VST3の正式対応や、ビルド時のexport設定を変更するわけではない。

規格: [VST3](https://github.com/steinbergmedia/vst3_pluginterfaces/blob/master/vst/ivsteditcontroller.h)、
[CLAP GUI](https://github.com/free-audio/clap/blob/main/include/clap/ext/gui.h)、
[CLAP主スレッドcallback](https://github.com/free-audio/clap/blob/main/include/clap/host.h)。

## 検証の境界

AudioGraph側ではG2の原本維持、文書の置き換え、重複、取消、worker/音声からの報告を検証する。
nice-plug側では模擬ホストを使い、CLAP初回activate・再ロード、VST3の接続待ち、
主スレッドへの配送、再入、拒否、非対応、破棄を検証する。
WindowsのVST3試験は実際のOSメッセージキューを使う。
これらの試験と、実DAWでエディターが表示されることの確認は別である。
