# plugin-host の構造と全公開API

更新日: 2026-09-12。監査基準 main `e2b629b` から契約修正後の実装へ更新済み。クレート版 `0.1.5`。

[共通データ・trait・寿命管理の全API](plugin-host-api-guide.md)と[監査結果](audits/plugin-host-review-2026-09-11.md)に続く、形式選択・ロード・探索・キャッシュ・editorの解説。ここでも、実装の動作と規格上の要求を分ける。

## 1. クレートの仕事

呼び出し側は「このpathのpluginをロードする」「パラメーターを読む」「editorを開く」と頼み、形式ごとの処理はこの窓口が選ぶ。VST3のCOM風interfaceやCLAPのC structを製品・CLIへ漏らさないためのfacadeである。

| 内部モジュール | 責務 | 評価 |
| --- | --- | --- |
| `lib.rs` | 公開export、thread初期化、window/APIの再export | 利用者が依存を一つにまとめられる入口として妥当 |
| `format.rs` | 対応形式の閉じた集合、拡張子、保存tag | backendの選択を局所化できる |
| `plugin.rs` | loaded instance、main API委譲、editorの統一 | format差と破棄順序を利用者から隠す目的に沿う |
| `scan.rs` | path探索、native factory列挙、stable IDによる解決 | metadata表現は共通化済み。ロードを伴う操作とpath探索が区別される |
| `catalogue.rs` | caller指定pathへの派生cache、stamp、再走査 | AudioGraphの設定方針から切り離されている。現在はnative scan実行も直接所有する |

外部依存は `plugin-host-api`、`vst3-host`、`vst3-host-view`、`clap-host`、`host-window`、`log`、`serde`、`serde_json`。dev-dependencyに同梱CLAP fixtureがある。AudioGraph製品・設定・engineへの依存はない。headlessなCLIでもwindow用crateは依存に入るが、eguiや製品UIへ依存することとは異なる。

`Format`の追加時に分岐を直す場所は一箇所だけではない。format、directory、scan、load、editor、diagnostic等のmatchをこのクレートと新backendに閉じ込める、というのが実際の効果である。

## 2. Module / Class / Instance / Processor を区別する

| 概念 | このAPIの表現 | 意味 |
| --- | --- | --- |
| module | path。cacheでは`catalogue::Module` | `.vst3`/`.clap`のfileまたはbundle。実コードを含む |
| class / plugin descriptor | `ClassInfo` | moduleが公開する「作れるpluginの種類」。module一つに複数あり得る |
| 保存参照 | `PluginRef` | format + stable ID + path hint。実instanceを所有しない |
| loaded instance | `Plugin` | classから実際に作った一個のplugin、main側窓口 |
| activation | `Processor` | そのinstanceを特定のAudioConfigで処理可能にした所有ハンドル |

VST3はfactoryからclassを列挙し、audio classとcontroller class等を区別する。CLAPのplugin factoryはdescriptorを列挙し、IDでinstanceを作る。file名とplugin IDは同じものではない。[VST3 factory][V1]、[CLAP factory][C1]、[CLAP plugin descriptor][C2]

```mermaid
flowchart LR
    D[探索directory] -->|installed_modules| M[format と path]
    M -->|scan_module| C[ClassInfo の列]
    C -->|reference| R[PluginRef]
    R -->|resolve_reference| P[path]
    P -->|Plugin::load + class ID| I[Plugin]
    I -->|activate| A[Processor]
```

この流れは説明用で、毎回全段階を通す必要はない。既知のpath/IDをロードするなら`Plugin::load`だけでよい。現在のload内部も一度scanしてclassを選び、native moduleを開き直してcreateする。

## 3. 形式識別

### `Format`

| API | 動作・理由 |
| --- | --- |
| `Vst3`, `Clap` | 両方をビルドに含める閉じたenum。形式追加を網羅的matchで検出する |
| `extension(self) -> &'static str` | `vst3` / `clap`。ドットは含まない |
| `tag(self) -> &'static str` | 保存用のstable小文字tag、`vst3` / `clap` |
| `from_tag(&str) -> Option<Format>` | 上記tagの完全一致。未知tagや大文字tagはNone |
| `from_path(&Path) -> Option<Format>` | 拡張子を大文字小文字無視で判定。`.dll`だけでは判定しない |
| `Display` | 人間向けに`VST3` / `CLAP` |
| `Serialize` | variant名ではなくstable tagを保存 |
| `Deserialize` | tagを読み、未知ならエラー |

`FORMATS: [Format; 2]` は `[Vst3, Clap]`。現在の表示・列挙順の定数であり、規格がこの順序を定めているわけではない。enumはDebug/Clone/Copy/PartialEq/Eq/PartialOrd/Ord/Hashも持つ。

`from_path` はファイルの実在・ABI・CPU architecture・内容を検証しない。拡張子は利用者が与えた選択の手掛かり。VST3にはdirectory bundleもあり、Windows/Linuxでは常に単体共有libraryという既存コメントは正確ではない。[VST3 packagingの公式説明](https://steinbergmedia.github.io/vst3_dev_portal/pages/Technical+Documentation/Locations+Format/Plugin+Format.html)、[CLAP entry][C3]

## 4. 探索と保存参照

### `ClassInfo`

| public field | 内容 |
| --- | --- |
| `format: Format` | plugin形式 |
| `id: String` | VST3はplatform-independent hexのclass ID、CLAPはdescriptor ID文字列 |
| `name: String` | plugin表示名 |
| `vendor: String` | vendor名 |
| `version: String` | plugin版文字列 |
| `category: String` | VST3 subcategories、CLAP feature tagsを`|`で連結した文字列 |
| `is_instrument: bool` | backendが分類情報から判断した音源かどうか |
| `path: PathBuf` | このclassを見つけたmodule path |

`reference(&self) -> PluginRef` はformat/ID/path/nameをコピーして保存参照を作る。class metadata自体はinstanceの寿命に依存せず所有できる。Debug/Clone/PartialEq/Eqを持つ。`ClassInfo`自身にserde実装はない。

### `PluginRef`

`format: Format`、`id: String`、`path_hint: PathBuf`、`display_name: String`の4field。検索の権威はformat+IDであり、表示名をidentityに使わない。pathが移動しても同じIDで探せるようにする独自host方針。両規格がこのRust保存形式を定義しているわけではない。

Debug/Clone/PartialEq/Eqを持つ。docにはserialized referenceとあるが、**この型自身はSerialize/Deserializeを実装していない**。保存形式への変換は現在の利用側が行う。

### 探索関数の全一覧

| API | 動作 | 費用・前提 |
| --- | --- | --- |
| `default_plugin_directories() -> Vec<(Format, PathBuf)>` | 両backendの慣例directoryを列挙。CLAP_PATHもbackend側で考慮 | FS/env参照あり。現在のbackendは実在directoryに絞る。ユーザー設定の読込ではない |
| `plugin_directories(&[PathBuf]) -> Vec<(Format, PathBuf)>` | 実在する各directoryを両Formatと組にし、sort/dedup | 引数が全対象。default directoryは勝手に追加しない。入力順の優先度はsortで維持されない |
| `find_modules(format, dir) -> Vec<PathBuf>` | 指定formatのmoduleをdir直下と一段下から探す | path探索のみ。任意深さの再帰scannerではない |
| `installed_modules(&[PathBuf]) -> Vec<(Format, PathBuf)>` | 上二つを組合せ、全候補をsort/dedup | 「見つかったpath」。libraryをロードした成功一覧ではない |
| `scan_module(path) -> Result<Vec<ClassInfo>>` | 拡張子でformat選択してscan | 第三者コードを実行する |
| `scan_module_as(format, path) -> Result<Vec<ClassInfo>>` | formatを明示してnative moduleを開きclass列挙、終了時にmodule解放 | VST3はaudio classだけ、CLAPはplugin descriptor。extension推測が不要 |
| `resolve_reference(reference, search_directories: &[(Format, PathBuf)]) -> Option<PathBuf>` | hintをロードしてID確認→同形式directoryの候補を順にロードし、最初の一致を返す | 未発見/走査失敗をNoneにまとめる。cacheは参照せず、複数libraryロードが発生し得る |

慣例directoryの代表はWindowsのCommonProgramFiles/ユーザーのPrograms/Common、macOSのsystem/user Library/Audio/Plug-Ins、Linuxのuser `.vst3`/`.clap` と `/usr/lib`・`/usr/local/lib`。実際の一覧はOSと環境変数、実在確認で決まる。[CLAP entryの探索規約][C3]

`resolve_reference`の戻りは所有されたloaded moduleではなくpathなので、その後の`Plugin::load`は改めてlibraryを開く。走査とloadの間でファイルが変わらないというtransactionも保証しない。戻ったpathをロードするときにもIDを指定するのが保存復元の基本である。

## 5. `Plugin` の全固有API

`Plugin`のfieldはprivate。内部`Backend` enumは、VST3の場合はeditor→plugin→module、CLAPの場合はplugin→moduleを保持する。VST3 editorは `vst3-host-view` が、CLAP editorは `ClapPlugin` 自身が持つ。この違いを利用側へ出さない。

現在はbackend内のnative instance guardもmoduleを保持するため、processorが `Plugin` より長生きしてもnative処理資源は残る。facadeのmodule fieldだけでその寿命を保証しているわけではない。editorを閉じることとprocessorを停止することも別である。

| API | 行うこと・理由 |
| --- | --- |
| `load(path, class_id: Option<&str>, context: Arc<dyn HostContext>) -> Result<Plugin>` | 拡張子で形式選択。ID指定ならそのclass、Noneなら列挙の最初。ファイル選択からのロードと保存復元を共通化 |
| `load_as(format, path, class_id, context) -> Result<Plugin>` | 明示format版。scanしてclass確認、native moduleを開き直してinstance作成 |
| `format(&self) -> Format` | 保持しているclassの形式 |
| `class(&self) -> &ClassInfo` | loaded class metadataへの借用 |
| `reference(&self) -> PluginRef` | classから保存参照を生成 |
| `name(&self) -> &str` | class表示名 |
| `format_interfaces(&self) -> Vec<&'static str>` | VST3のinterface名/CLAP extension名。診断用。stable保存IDや動作分岐の契約ではない |
| `has_editor(&self) -> bool` | backendにeditorがあるか問い合わせ。開けるOS実装まで保証しない |
| `open_editor(&mut self, owner: *mut c_void) -> Result<(), String>` | 自前のtop-level containerへnative editorをattach。ownerは親hostのroot window、standaloneならnull |
| `close_editor(&mut self)` | editorを解除・破棄。plugin本体やprocessorは停止しない |
| `editor_is_open(&self) -> bool` | editorオブジェクトを保持しているか。OS側のclose要求はtickで反映 |
| `editor_window(&self) -> Option<&ContainerWindow>` | containerを借用。自分でmessage loopを持つharness等に公開 |
| `tick(&mut self)` | VST3/CLAPのmain通知を配送し、editor resize/closeも反映。CLAPのmain callback・timer・Linux fdも処理。記述の更新完了はrefresh_metadataで判断 |
| `Debug` | format/nameを表示し、内部native情報は展開しない |

ロード、editor操作、tickはowner/main側の操作。`Plugin`はCloneできない。`tick`はeditorを開いていない場合も必要。pluginが `on_main_thread` 等に依存しているためである。[CLAP host callback][C4]、[CLAP timer][C5]

editorはVST3では `IPlugView` のattach/removeとframe経由resize、CLAPではgui extensionのcreate/set_parent/show/hide/destroy等へ対応する。native window pointerをこのAPIへ出すのはOS UI統合の境界で、共通音声payloadがnative pointerを持つこととは区別する。[VST3 IPlugView][V2]、[CLAP GUI][C6]

### `Plugin` が実装する `SubPluginMain` の全メソッド

以下はすべて、`Backend`を選んで同じ名前のtraitメソッドへ委譲する。facadeが独自にparameter値・latency・state形式を再解釈しないことに意味がある。利用するにはtraitをscopeへ入れる。

| メソッド | 共通API解説の項目 |
| --- | --- |
| `tick()` | main保守。inherent methodと同じ |
| `refresh_metadata()` | 更新完了または停止必要を返す |
| `request_main_bus_channels(input, output)` | inactiveで明示的に幅を要求 |
| `note_end_ports()` | native voice終了を報告できる入力port |
| `params()` | 全parameter記述の借用 |
| `capabilities()` | instanceの能力 |
| `voice_info()` | optional voice情報 |
| `note_dialects()` | 診断用note方言 |
| `io_layout()` | 全I/O情報 |
| `snapshot()` | 全parameter値 |
| `param_to_text(id, plain)` | 表示文字列化 |
| `param_from_text(id, text)` | 文字列解析 |
| `set_param(id, plain)` | main側のparameter編集 |
| `save_state()` | opaque state保存 |
| `load_state(data)` | inactive状態で復元 |
| `latency_samples()` | 保持したreported latency |
| `activate(config)` | 所有するProcessorを作る |

全シグネチャ・backend動作・呼び出し条件は[共通traitの表](plugin-host-api-guide.md#3-管理処理通知のtrait)を参照。facadeを使っても、そこに記載した現状の制限は消えない。

## 6. `catalogue` モジュール

このモジュールだけは `plugin_host::catalogue` として公開される。保存先を自分で選ばず、呼び出し側が渡す。AudioGraphのpins、設定file、defaultの復元等は担当しない。

### データ型

| 型・public field | 意味 |
| --- | --- |
| `Class.id: String` | stable plugin/class ID |
| `Class.name: String` | 表示名 |
| `Class.category: String` | 形式由来の分類文字列 |
| `Class.is_instrument: bool` | 音源分類 |
| `Module.path: PathBuf` | native moduleのpath。これはlibrary handleではない |
| `Module.format: Format` | 形式 |
| `Module.stamp: Stamp` | scan時点の変更検出情報 |
| `Module.classes: Vec<Class>` | そのmoduleで列挙したclass。vendor/versionはこの縮約型に含まない |
| `Module.error: Option<String>` | 通常のload/scan失敗理由。Noneならその種の失敗なし |
| `Stamp.modified: u64` | file、またはbundle内の最新mtime、Unix epochから秒 |
| `Stamp.size: u64` | fileサイズ、またはbundle内fileサイズ合計、byte |

`Class`/`Module`はDebug/Clone/PartialEq/Eq/Serialize/Deserialize。`Stamp`はそれらにCopy/Defaultも持つ。Moduleのerrorは読込時に省略可能、Noneは保存時にも省略する。

`Module::file_name() -> String` はpathの最後の要素をlossy Unicodeで返す。plugin本来の表示名ではない。先頭class名と取り違えないための区別でもある。

### 操作の全一覧

| API | 実際に行うこと・失敗時 |
| --- | --- |
| `stamp_of(path) -> Stamp` | fileのmtime/size、directory bundleはtreeを歩き最新mtimeと合計size。読めないものは寄与させず、全体が読めなければ0/0 |
| `cached(path) -> Vec<Module>` | JSON cacheを読むだけ。欠落/読めないfileは空、不正JSONは警告して空。native pluginを実行しない |
| `refresh(directories, cache_path: Option<&Path>) -> Vec<Module>` | 候補pathを探索し、path/format/stamp一致分を再利用、変更分をnative scan。消えたmoduleは結果から除き、path順に並べて保存。Noneなら永続cacheなしでもscanする |
| `forget(path) -> Result<(), String>` | cache fileを削除。既にない場合は成功。次回refreshで再scanさせるため |

mtime秒+byte数は安い変更検出のheuristicであり、内容hashではない。同秒・同sizeの差し替え等を完全検出しない。これは派生cacheの再取得方針としての選択で、plugin identityを保証する仕組みではない。

内部の`Cache`と`store`は非公開。`store`は親directoryを作り、整形JSONを隣接した`.json.new`へ書いてrenameする。file破損を減らす意図はあるが、複数refresh同時実行の調停やtransaction APIはない。保存失敗は警告し、メモリー上のscan結果は返す。

### スキャンの実行境界

`refresh`は第三者コードを同期的に同一process内でロードする。「UI以外のthreadで呼ぶ」は画面を止めにくくするが、process crashやnative hangから隔離するものではない。`Module.error`へ記録できるのはRustのResultとして戻った失敗であり、crash/hangしたmoduleの失敗を現実装が回収して記録するわけではない。

同じbackend内でnative binaryが別の所有threadに使用されている場合は、ロードを`HostError::ModuleBusy`で拒否する。`refresh`はこれを故障としてcacheに保存せず、既存entryがあれば古いstampのまま保持し、なければその回の結果から除外する。所有者が解放した後のscanで再試行できる。同じ所有threadからのロードは既存moduleを共有する。

安全なpath列挙、cache保存、native scan実行は概念として区別できている。任意のinstalled pluginを扱うscannerとして頑健にする場合は、module単位のworker process、timeout、途中結果保存を別の実行責務として導入する余地がある。これは全面的なaudio processingのIPC化とは別の変更単位。[監査の条件付き設計課題](audits/plugin-host-review-2026-09-11.md#scanner)

## 7. スレッド初期化と共通API再export

`init_thread() -> Result<ThreadGuard>` はVST3 backendの `init_apartment()` を呼び、成功後に `reclaim_main_thread()` を実行する。`ThreadGuard`は`vst3_host::ApartmentGuard`の別名で、Send/Syncではない。Windowsでは成功した`OleInitialize`一回分を所有し、Dropで返却済みprocessorを回収してから`OleUninitialize`を一回呼ぶ。既存STA上の複数guardもそれぞれの参照を解放する。既存MTAは初期化エラーになり、そのapartmentを解除しない。他OSではCOM操作をせず、Drop時の回収だけ行う。

呼び出し側は `let _thread = plugin_host::init_thread()?;` として、plugin・editor・processorより先にguardを作り、それらの解放と所有threadへの返却完了まで保持する。初期化helper内だけで保持しても、helperのreturn時に解除されるため不十分。guardは任意のロードAPIに必須のtokenではなく、OSのmain threadであることや全資源の先行破棄を型で証明するものではない。

`plugin-host`から利用可能な共通APIは以下。定義・理由・全メソッドは[plugin-host-api解説](plugin-host-api-guide.md)で一度説明している。

| 分類 | 再exportされた全項目 |
| --- | --- |
| 音声 | `AudioBuffers`, `AudioConfig`, `AuxBuses`, `BufferLayout`, `MAX_AUX_BUSES` |
| 記述 | `BusInfo`, `IoLayout`, `Capabilities`, `VoiceInfo`, `ParamId`, `ParamFlags`, `ParamInfo`, `ParamSnapshot`, `ParamValue` |
| イベント・時間 | `Event`, `EventSink`, `NoteEvent`, `NoteExpression`, `NoteId`, `ParamEvent`, `Target`, `TimeContext` |
| 契約 | `HostContext`, `SubPluginMain`, `SubPluginProcessor`, `ProcessStatus`, `RestartReason`, `MetadataUpdate` |
| 寿命 | `MainThread`, `Processor`, `reclaim_main_thread`, `ThreadGuard` |
| エラー | `HostError`, `Result` |

「全て再export」という既存説明には例外がある。`note_id_from_wire`、`note_id_to_wire`、doc-hiddenな`bitflags_lite!`はこのfacadeからexportされない。内部module名もexportしない。

## 8. window再exportから使える全API

これらの実装所有者は `host-window`。今回の主監査範囲にOS実装全体は含めないが、`plugin-host`の公開面から呼べるため全methodを説明する。

### `Size`

`Size { width: i32, height: i32 }`。`Size::new(width, height)`で構築。Debug/Clone/Copy/PartialEq/Eq/Defaultを持ち、Defaultは0×0。正値や最大サイズの検査はしない。

### `ContainerWindow`

| API | 動作・条件 |
| --- | --- |
| `new(title: &str, size: Size, owner: *mut c_void) -> Result<Self,String>` | native editorを入れる空containerを生成。message pumpを所有するthreadで呼ぶ。ownerはroot windowまたはnull |
| `platform_handle() -> *mut c_void` | native attach/set_parentへ渡すhandle |
| `state() -> &Rc<WindowState>` | close要求とsizeの共有状態を借用。`WindowState`自体はfacade直下には再exportされない |
| `set_client_size(Size)` | stateとOS client areaを更新 |
| `client_size() -> Size` | 保持しているclient size |
| `show()` | window表示 |
| `scale_factor() -> f64` | display scale。1.0が基準倍率 |
| `close_requested() -> bool` | ユーザーがcloseを要求したか。native viewの解除は所有者の仕事 |
| `Drop` | native windowを解放。native plugin viewの解除順序はeditor所有者が先に整える |

戻り型経由で触れる `WindowState` は `close_requested: Cell<bool>`、`size: Cell<Size>`、Defaultを持つ。単なる読み取りsnapshotではなく、公開Cellで変更できる。上位が書き換えるとeditorの内部判断へ影響するため、`editor_window()`は完全な隠蔽境界ではなく低水準のharness用出口でもある。

現`host-window`はWindowsとX11に実装があり、macOS等はstubである。plugin audioの利用可能性と、このcontainerでeditorを表示できるかは別に確認する。

### `Key`

| variant/API | 意味 |
| --- | --- |
| `Letter(u8)` | 大文字ASCII A〜Z |
| `Digit(u8)` | ASCII 0〜9 |
| `Function(u8)` | 数値1〜24 |
| `Space`, `Enter`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown` | 各キー |
| `letter(char) -> Option<Key>` | ASCII英字のみ受け、uppercaseへ変換 |
| `digit(char) -> Option<Key>` | ASCII数字のみ |
| `function(u8) -> Option<Key>` | 1〜24のみ |

Debug/Clone/Copy/PartialEq/Eq。直接variantを作れば上記範囲外値も入る。modifier状態やIME文字列を持つ完全なkeyboard eventモデルではない。

### 関数

| API | 役割と使う場所 |
| --- | --- |
| `forward_key(window: usize, key: Key, pressed: bool)` | root windowへkey down/upを送る。0 handleは無視。editorが消費しなかったtransport shortcut等の転送 |
| `root_window(handle: *mut c_void) -> *mut c_void` | 渡されたviewの所属root windowを求める。nullはnull |
| `poll()` | 自分たちが所有するwindow event源だけ進める。X11では必要、Windows等ではhostが配送するのでno-op |
| `pump_events()` | 呼出threadのmessage queueをdispatch。standalone harness向け。pluginとしてDAW内で動く場合はDAWのqueueを勝手に回さず`poll`を使う |

これらはhostのOS統合であり、VST3/CLAPの共通audio規格APIではない。キー転送などの方針を採用するかは利用側が決める。

### main loopで必要な三つの操作

| 操作 | 進める対象 |
| --- | --- |
| `poll`（standaloneなら必要に応じ`pump_events`） | OSのwindow event |
| 各`Plugin::tick` | そのpluginのmain callback/timer/editor等 |
| `reclaim_main_thread` | 返却済みprocessor・main資源のnative停止/破棄 |

一つ呼べば他二つも必ず済むわけではない。この区別はheadless hostや終了処理を実装する際に重要。

## 9. 読む順序と利用の流れ

構造を把握するには `plugin-host-api/src/traits.rs` → `buffers.rs` / `params.rs` / `events.rs` → `ownership.rs` → `plugin-host/src/plugin.rs` → `scan.rs` → `catalogue.rs` の順が読みやすい。共通型の変換根拠が必要になったところでbackendへ進む。

典型的なstandalone処理の順序は次のとおり。

1. 所有threadで`init_thread()?`のguardを保持し、host側の `HostContext` を作る。guardは全plugin資源の解放・回収後まで保持する。
2. 必要ならpath候補だけ列挙。classを知る必要がある候補だけ`scan_module`。
3. `Plugin::load`へpath/ID/contextを渡す。
4. 必要なstate復元をinactiveで行い、記述情報を確認する。
5. `AudioConfig`を決めてactivate。blockに必要な音声とイベントの容量を処理前に用意する。
6. 所有権を渡したProcessorを排他的にprocess。main側では通知とtick、必要なwindow処理を進める。
7. 終了時に処理を止めてProcessorを返し、ownerで回収。editor/Plugin/contextも適切な順に閉じる。

現APIの安全性・通知・event容量の不足は[監査結果](audits/plugin-host-review-2026-09-11.md)に記載した。この流れを守るだけで任意の第三者pluginの挙動まで保証できるわけではない。

## 10. 仕様参照

CLAP/VST3は共通API解説と同じcommitへ固定した。パス探索・cache・root window選択・owner-thread回収は、このリポジトリのhost設計であり規格そのものではない。

[C1]: https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/factory/plugin-factory.h
[C2]: https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/plugin.h
[C3]: https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/entry.h
[C4]: https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/host.h
[C5]: https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/ext/timer-support.h
[C6]: https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/ext/gui.h
[V1]: https://github.com/steinbergmedia/vst3_pluginterfaces/blob/4f547e8e102b47de4a8b8aaf343c73b700786372/base/ipluginbase.h
[V2]: https://github.com/steinbergmedia/vst3_pluginterfaces/blob/4f547e8e102b47de4a8b8aaf343c73b700786372/gui/iplugview.h

メタデータ更新とRT出力契約の利用手順・検証結果は[契約修正記録](audits/plugin-host-contract-fixes.md)を参照。`MetadataUpdate`も共通APIから再exportする。
