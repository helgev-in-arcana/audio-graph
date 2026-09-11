# plugin-host / plugin-host-api 構造監査

確認日: 2026-09-11。対象: `main` `e2b629bf18bcda6d23d1abe436451a9ac0e0c996`。PR #82のmerge、ローカルmainとorigin/mainの一致を確認した。

2026-09-12追記: F1〜F5は実装済み。以下は修正前の監査記録を保持している。[修正内容・コミット・行数・検証結果](plugin-host-contract-fixes.md)を参照。

## 1. 結論

**モジュール分割とコンパイル時の依存方向は概ね妥当。ただし、共通APIが保証する処理条件と、通知・規格差を吸収する責務には修正すべき境界が残る。** 新しいクレート分割より、既存の契約とその実装を揃える作業を優先する。

今回の優先事項は5件。個別規格の機能を増やす話ではなく、既に公開されているAPIを形式に依存せず安全に使うための条件である。

| ID | 優先度 | 構造的な問題 | 主な責務の所在 |
| --- | --- | --- | --- |
| [F1](#f1) | P1 | 音声ブロックとactivation構成の一致がsafe API境界で保証されない | apiのbuffer/processor契約、両native backend |
| [F2](#f2) | P1 | 出力イベントの容量超過・RT予算を共通APIが表現できない | apiのEventSink、backend出力変換、callerの準備 |
| [F3](#f3) | P2 | host通知の実行threadと配送責任が一貫していない | HostContext、native callback shim、main側service |
| [F4](#f4) | P2 | 動的metadataの無効化→再取得→公開の完了境界がない | SubPluginMain、Plugin::tick、backend cache |
| [F5](#f5) | P2 | 規格で得た事実と、上位のvoice回収方針が同じNoteEndへ混ざる | 共通event意味、backend変換、上位voice管理 |

別に、[native scannerの障害隔離](#scanner)は用途に応じて設計判断が必要。現在の同一process scannerをどこまで製品の信頼境界として許容するかで、着手優先度が変わる。

詳細な構造と全公開APIの説明は、[plugin-host-api](../plugin-host-api-guide.md)、[plugin-host](../plugin-host-guide.md)にまとめた。型・field・variant・methodに加え、window再export、隠れた公開macro、標準traitも対象にした。

## 2. 監査の範囲と方法

- 対象2クレートの全src、README、Cargo.toml、unit/integration testsを読んだ。
- 契約の成立確認に必要な範囲で、`vst3-host`、`clap-host`、`host-window`と呼び出し側を辿った。これらの隣接クレートを全面監査済みとはしない。
- 公式VST3/CLAP仕様ヘッダー28ファイルを取得し、commitとSHA-256をtempへ保存した。現在のbinding依存は`vst3 0.3.0`、`clap-sys 0.5.0`。
- メモと確認用コードを現在のmainワークツリーのtempへ置いた。製品実装・既存テストの修正、commit、pushは行っていない。
- 局所的な変換ミスや容易に後から足せる機能不足は、構造的findingとは別の注意事項にした。

## 3. 大局から見た責務・依存・隠蔽

| 観点 | 現状の評価 | 理由 |
| --- | --- | --- |
| apiとfacadeの分担 | 良い | 共通語彙/trait/所有権はapi。形式識別・load・scan・editor選択はhost |
| api内の分割 | 良い | buffers/events/params/traits/ownershipで関心がまとまり、循環したモジュール依存を新たな問題としては認めない |
| host内の分割 | 概ね良い | format/plugin/scan/catalogueが明瞭。catalogueにnative scan実行が直接含まれる点は障害隔離と関係する |
| コンパイル依存 | 良い | apiは外部依存なし。hostはbackend/api/windowへ依存し、AudioGraph製品や設定へ逆依存しない |
| 注入されたhostサービス | 方針は良い | HostContextへの逆向きcallbackは依存逆転として自然。ただしthread/配送契約が不十分 |
| native型の隠蔽 | 概ね良い | PluginのBackendはprivate。native audio pointerやCOM型を共通値payloadへ出さない。OS window handleはUI統合の明示的な出口 |
| ownershipの配置 | 妥当 | 共通のProcessor返却・MainThread破棄機構をapiで共有する目的がある。別crate化は不要 |
| 製品方針との独立性 | 改善済み部分と残存部分がある | 設定path/pins/グラフ再構成は製品側。NoteEnd合成は依然として上位のvoice回収方針を背負う |
| 公開面の最小性 | 小さな改善余地 | doc-hidden macroのexport、mutable WindowStateへの出口、raw fieldの自由度はあるが、これだけで大規模再編は勧めない |

### 直前のP1修正との関係

main/processorのnative資源保持、返却先instanceを取り違えないdeactivate、owner thread破棄、グラフ計画とprocessor構成の更新境界は今回のmainに入っている。今回の確認でも、その改善を取り消す根拠は見つからなかった。

F1は「製品がgraphとprocessorを一組で更新するか」ではなく、**汎用safe process APIに任意のbufferを渡したとき、nativeが使う寸法と一致するか**という別の条件。F2もProcessorハンドル自体の無確保性ではなく、出力イベントの収集まで含む処理経路の問題である。

以前保留した共通契約の追加crate分離を、今回の解決条件として復活させる必要はない。

<a id="f1"></a>
## 4. F1 — [P1] 音声ブロックとactivation構成を結び付ける検査がない

**問題の鳥瞰:** activationは固定I/O構成を作るが、processへ渡すAudioBuffersは独立した申告値を持つ。safeな公開APIの後で、その二つのどちらを信じるかが分かれている。

`AudioBuffers::new`は渡されたslice長を、その呼び出しで申告したchannel数とframe数に対して検査する。`Processor::process`はそのまま委譲する。一方、両native backendはactivate時のconfig/bindingからchannel pointerを作り、blockのlayout・channel数・aux配置との一致を調べない。framesの最大値検査はあるが、これだけでは足りない。

根拠: [AudioBuffersの構築](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host-api/src/buffers.rs#L152-L180)、[Processor委譲](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host-api/src/ownership.rs#L222-L238)、[VST3 pointer構築](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/vst3-host/src/plugin.rs#L861-L894)、[CLAP pointer構築](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/clap-host/src/plugin.rs#L1098-L1139)。規格側も実際のbus構成とbufferの対応を要求する。[VST3 AudioBusBuffers](https://github.com/steinbergmedia/vst3_pluginterfaces/blob/4f547e8e102b47de4a8b8aaf343c73b700786372/vst/ivstaudioprocessor.h)、[CLAP process](https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/process.h)

**影響:** stereoでactivate後、mono分しかないsliceをmonoと申告すれば、Rust側の構築検査は通るがnative側は2channel分を扱おうとする。呼び出し側のsafeコードから領域外アクセスへ進める構造で、単なる不正入力時の音質低下ではない。Interleavedを表現できるのにnativeがPlanarとして扱う点も同じ契約の穴。

**確認:** stereo/4framesでactivateし、入力・出力の実sliceは各8sample確保したまま、AudioBuffersのmetadataだけmono/4framesとした。`Continue`となり8sampleすべてを書き換えた。つまり申告上の出力領域4sampleを超えて処理された。sliceの実長とallocationには余裕を持たせ、実際の領域外アクセスは実行していない。さらに短いsliceの危険性はコード経路からの判断。

**推奨:** native pointerを作る手前で、一度検証したactivation構成に対してblockを検証する責務を設ける。少なくともPlanar、channel合計、aux区切り、frame上限、checked arithmeticによる必要slice長を確認し、不一致はnativeへ渡さない。共通helperを両backendから使う案と、configを保持する共通Processor入口で検証を必須にする案がある。後者はAPI変更が大きいので、既存構造で強制できる最小の方法を先に比較する。

| 変更範囲 | 内容 |
| --- | --- |
| `plugin-host-api` | config/bufferの妥当性検査とprocessの不一致時契約 |
| `vst3-host`, `clap-host` | raw pointer形成より前に検査を必ず通す |
| caller | 正常な既存callerの流れは維持。不正構成への結果処理を揃える |
| 検証 | 正常main/aux、寸法不一致、Interleaved、短いslice、過大値をnativeに到達させない試験 |

実行時費用は各blockの固定個数の比較・加算で、検査箇所次第でaux最大3件の走査を含む。確保や待機は不要。現状のままcallerの規律に頼る案は変更不要だが、safe公開APIとしての穴を残すため非推奨。

<a id="f2"></a>
## 5. F2 — [P1] EventSinkの容量超過がリアルタイム契約の外にある

**問題の鳥瞰:** 音声スレッドで使う出力口なのに、容量が処理予算ではなくVecの初期予約量でしかない。backendが何件出すかと、callerが何件用意するかを共通契約が結び付けない。

`with_capacity`のdocは事前確保を説明するが、`push`は無条件のVec::push。CLAPのnative出力collectorは満杯時に拒否できても、EventSinkへ移す際には再確保できる。VST3もNoteEnd合成等で同じpushを通る。

根拠: [EventSink](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host-api/src/events.rs#L453-L487)、[CLAP共通sinkへの転送](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/clap-host/src/events.rs#L515-L522)、[製品の容量256](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/audio-graph-plugin/src/plugin.rs#L302)。CLAPのnative collector上限は現実装で2,048なので、製品の予約256との間にも差がある。

**確認:** capacity 1のsinkへ1,000eventsをpushし、allocation/reallocationを計測した。全1,000件が残り、9回の確保・拡張を確認。これはnative plugin固有挙動を必要としないAPI単体の再現である。

**影響:** 通常は事前確保範囲で動くため、少量イベントの試験では見えない。イベント集中時に処理スレッドでallocatorへ入り、遅延や音切れを誘発し得る。既存のProcessor無確保試験は出力eventsを増やさないfake DSPであり、このケースを保証していない。

**推奨:** boundedな出力口として、満杯時に拡張しない `try_push` と容量超過の結果を設ける。容量準備はmain側、処理中の超過は検出可能にする。NoteOff/NoteEndの欠落は声の残留にも関わるため、単に黙って捨てるだけで完了にせず、callerが再同期/reset等を選べる状態を渡す。

変更範囲はapi EventSink、両backendの転送、adapter/製品/CLIのsink準備と超過処理。全入出力イベントを新しい汎用message systemへ移す必要はない。実行時には一件ごとの容量比較が増えるが、超過時の再確保をなくす。現状維持・予約量増加だけでは「上限を超えない」という根拠が残らない。

<a id="f3"></a>
## 6. F3 — [P2] HostContext通知のthread・配送責任が揃っていない

**問題の鳥瞰:** plugin→hostの通知を共通traitにまとめた一方、どのthreadから届き、どこまでその場で実行してよいかが統一されていない。

HostContextは「全methodはmain thread」と説明する。しかしCLAP `request_restart` はthread-safeでaudio側からも呼べ、現shimはcontext.request_restartをそのまま呼ぶ。これに対してcallbackやtimer等の別経路はatomicへ記録してtickで配送する。同じ制御経路内で延期するものと外部へ同期通知するものが混ざる。

根拠: [共通契約](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host-api/src/traits.rs#L29-L60)、[CLAP restart直通](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/clap-host/src/host.rs#L389-L406)、[CLAP hostのthread-safe指定](https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/host.h)。

**影響:** 共通docを信じて、受信時にmain資源へアクセスしたり再構成・ログI/O・待機を実行するhostを安全に書けない。Send+Syncはメモリー共有の条件であり、main配送やRT非待機性の保証ではない。現在のAudioGraphは主としてatomic通知に留めているが、汎用hostの契約としてその利用法を強制・説明できていない。

同じ逆向き経路には値の意味の不一致もある。`param_edited(id, plain)`にVST3はnative normalized値を渡す。CLAPはprocess出力sinkとinactive flushに分かれ、flush出力は現在clearされる。後者の個別変換修正は局所作業だが、どの通知がどこへ届くかを先に決めないと規格ごとに異なる修正を積み増すことになる。

**推奨:** native threadから受ける要求記録と、main側でユーザー実装へ渡す制御通知を分ける。第一候補はbackendがthread-safe要求を軽量に記録し、mainのservice/tickがまとめてHostContextへ配送する形。生のaudio出力eventはEventSinkに保持する。そのうえでGUI編集を採用するか、automationへ伝えるかはhost利用側が決める。

代案は `HostContext` を明示的なany-thread/RT通知窓口として再定義し、すべての実装者に非待機・非再入の条件を課すこと。ただし型のSend+Syncだけでは条件を強制できず、host名・main側GUI通知まで一緒に扱う理由も弱くなる。doc一行だけを修正して直通を維持する案は最小差分だが、利用者負担と実装監査の負担が残る。

変更はtraits、CLAP/VST3 shim、main service、HostContext実装者。callback再入中のactivate等を防ぎ、任意threadからのrestartがmainへ一度だけ安全に届く試験が必要。audio側は既存atomic記録を利用できるため、処理ブロックごとの重い同期を増やす必要はない。

<a id="f4"></a>
## 7. F4 — [P2] 動的metadataの再取得を完了させる共通操作がない

**問題の鳥瞰:** `RestartReason` は変化を通知するが、共通APIには「必要な停止後にbackendの記述を更新し、それから新しいparams/I/Oを取得する」という境界がない。getterを呼び直せばよいという契約にも、再activateで全て更新される契約にもなっていない。

現実装ではCLAPのparamsはtickで再取得する一方、audio portsの保持値はcreate時に読んだものを使い続ける。VST3のparamsもcreate時のcacheで、title変更通知後にこれを更新する共通経路がない。`SubPluginMain`を実装したfakeでは成立しても、nativeのcache状態が置き去りになる。

さらにVST3の`io_layout(&self)`は、inactiveならmono mainをstereoへ変更提案してから記述を返す。読み取りに見える共通methodが、形式固有の構成方針まで実行する。このため「現構成の観測」「構成を選ぶための問い合わせ」「構成の変更」の境界も曖昧である。[stereoの提案とgetter](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/vst3-host/src/plugin.rs#L227-L327)

根拠: [SubPluginMainのgetter群](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host-api/src/traits.rs#L83-L115)、[CLAPロード時取得](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/clap-host/src/plugin.rs#L235-L236)、[CLAPの通知適用](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/clap-host/src/plugin.rs#L394-L436)、[VST3の初回取得](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/vst3-host/src/plugin.rs#L141)。

CLAPのport一覧・channel数等の変更はinactiveで処理すべき項目があり、parameter再scanにも変更種別ごとの条件がある。通知が来たというだけでactive中に全getterを再実行するのも正しくない。[CLAP audio ports](https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/ext/audio-ports.h)、[CLAP params](https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/ext/params.h)

**影響:** hostが正しく停止・再構成を選んでも、共通getterから古いparameter/bus情報を受け取り、それで次の構成を作り得る。直前に導入したgraph/processorの整合更新は、その入力metadataが正しく更新されていることまで保証しない。

**推奨:** main側に更新種別を受けてmetadata更新を完了する明示的な操作を置く、または既存serviceに「停止が必要か・更新済みか」を返す結果を持たせる。backendがnative条件に沿ってcacheを再構築し、callerは更新済み記述を受けて再activateする。全metadataを無条件に各blockで読み直す方法は採らない。

この整理と併せ、getter内のstereo優先方針は明示的な構成要求/交渉へ分離する。backendには規格に従った交渉機構、callerには望む構成を選ぶ方針を残す。

変更範囲はapiのmain契約、facade委譲、両backendのcache/通知、adapter/製品の再構成順序。F3の通知配送と近いが、F3は通知の到着条件、F4は通知後の更新完了条件なので区別して検証する。費用は主にpreset・parameter集合・I/O構成等の変更時。現状維持なら動的変更を対応能力として広く名乗らず、非対応項目を制限する必要がある。

<a id="f5"></a>
## 8. F5 — [P2] NoteEndにnativeの事実と上位の回収方針が混ざる

**問題の鳥瞰:** 共通 `NoteEnd` は「pluginがvoice終了を知らせた」ことを表す。しかしVST3には同等の通知がないため、backendが入力NoteOffを受けた時点でNoteEndを合成する。得られない情報を、上位の都合に合わせた確定情報へ置き換えている。

[共通NoteEnd](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host-api/src/events.rs#L137-L146)、[入力NoteOffからの合成](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/vst3-host/src/vst_events.rs#L238-L268)、[native出力NoteOffの変換](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/vst3-host/src/vst_events.rs#L217-L235)が根拠。合成関数の説明も「graphが待ち続けないため」という上位の回収事情を根拠としている。

CLAPのNOTE_ENDはvoice寿命をpluginからhostへ知らせる協調機構で、NOTE_OFFとは意味が違う。VST3のNoteOffEventをこの終了通知と同一視することは規格の要求ではない。[CLAP event model](https://github.com/free-audio/clap/blob/a47f6badb49d948fd009998f28309cdab78979c9/include/clap/events.h)、[VST3 events](https://github.com/steinbergmedia/vst3_pluginterfaces/blob/4f547e8e102b47de4a8b8aaf343c73b700786372/vst/ivstevents.h)

**影響:** generic callerは「本当に終了したvoice」と「キーが離され、終了したと仮定して回収するvoice」を区別できない。release tail中のvoice状態を保持する判断も、NoteOff時点で回収して資源を制限する判断も上位の選択だが、現状はbackendが一律に後者を選ぶ。さらにpluginが出力する音楽イベントのNoteOffを、入力voiceの終了と同じ列へ混ぜている。

**推奨:** native由来のNoteOff/NoteEndの意味を保ち、voice終了通知を取得できるかを能力として示す。VST3でNoteOff時に上位状態を回収する互換方針が必要なら、adapter/engineのvoice寿命方針として明示して実装する。別案は終了の根拠をeventに持たせ、推定と確定を区別すること。どちらも規格判定を製品のあちこちへ散らす必要はない。

変更範囲はapiのevent/能力契約、VST3出力変換、adapter/engineのvoice回収。CLAPのnative NOTE_END経路は意味を維持する。推定をbackendに残したままdocを弱める現状維持案は小さいが、既にNoteOff型があるのに別名のNoteEndへ変換する理由が曖昧になり、非AudioGraph利用者へ方針を押し付けるため非推奨。

## 9. 規格対応上の注意点 — 主findingと分けるもの

以下は全APIを理解するうえで重要だが、個別変換の修正や将来の表現拡張が中心なので、上の5件に数えない。既に問題がないという意味でもない。backend内監査で実物の対応範囲を確定する際の入力とする。

| 項目 | 現状・判断 |
| --- | --- |
| VST3 parameter編集のplain/normalized | `param_edited`にnormalizedを渡す。共通契約との不一致。F3で通知の出口を整理したうえで変換を修正する候補 |
| VST3 note expression | Tuning/VolumeはCLAPとnative値域が異なるが、現実装はvalueをコピー。重大な音楽的意味の違いだが、変換処理自体はbackend局所で是正できる |
| VST3 stepped parameter | min/maxを0〜stepCountとし、default/snapshotはnative plain変換する。単なるstep数とplain値域を混同しない |
| VST3 `ParamInfo.module` | 実際はunits文字列で、unit階層pathではない。名前と仕様対応を直す余地 |
| `ParamInfo::normalize` | 線形比率。native正規化ではない。現VST3の処理は別ParamMapを使うため、全parameter処理が線形で誤っているわけではない |
| Modulate / target | VST3のModulateは捨て、SetValueの非Global targetはglobal化。READMEの「modulationを足す」と異なる。対応不能時の方針を公開契約に明記すべき |
| 能力の粒度 | POLY_MODULATABLEは複数scopeのOR。scopeごとの可否や全note expression対応を表せない。既存boolを完全な許可証に使わない |
| ProcessStatus::Silent | CLAP sleepとVST3の今回のmain出力無音を一緒に扱う。将来の処理停止最適化へ使う前に観測/継続方針を分離。現在のadapterは返値を利用して停止していないため、停止バグを実証したとはしない |
| TimeContext | 多くの値にunknownがない。VST3 system-time-validと未設定値の不一致はbackend側の局所事項 |
| VoiceInfo | CLAP queryのactive条件と現ロード時取得の差。capacityは現在の確保容量で、生涯最大値ではない |
| バス/イベント表現の上限 | aux3、f32、主にmain-first、note portはbool、Targetは4要素addressの部分集合。SysEx/MIDI2/NoteChoke等は公開モデルにない。これらを全部実装する提案にはしない |
| error型 | coreはHostError、editor/cache操作にはStringもある。現在の小さな境界だけを理由に大規模な共通error体系は不要 |
| thread初期化 | facadeはinit_threadを冪等と説明するが、Windows backendは毎回OleInitializeし、対応する終了操作を持たない。DAW内での利用指示も揃っていない。host-window/OS統合の監査で確認する |

全API解説には、これらの実装上の差も明示した。修正済みの保証として読まれないよう、仕様上の正しい意味と現実装の動作を分けた。

<a id="scanner"></a>
## 10. 条件付き設計課題 — scannerの障害隔離

`catalogue::refresh`はpath列挙・cache再利用・native scan・保存を同期的にまとめる。scanがResultを返せば失敗を記録できるが、native crash/hang時は最後の保存まで到達できない。製品のbackground thread起動もprocess障害を隔離しない。[refresh](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/plugin-host/src/catalogue.rs#L174-L210)、[製品の起動](https://github.com/helgev-in-arcana/audio-graph/blob/e2b629bf18bcda6d23d1abe436451a9ac0e0c996/crates/audio-graph-plugin/src/editor.rs#L234-L247)

以前の作業ではinstalled VST3をロードするテストの停止を観測しており、このターンでは同じ無差別ロードを再実行していない。原因や特定pluginの欠陥まで確認した記録ではないが、scanner実行を回収できるかという責務を検討する材料にはなる。

| 選択肢 | 費用・結果 |
| --- | --- |
| 同一processを維持 | 変更最小。信頼するpluginを対象にする用途には使える。crash/hang隔離は提供しないと明示 |
| module単位の別process scan | worker起動・結果protocol・timeout・途中保存が必要。製品を落とさず候補ごとの失敗を扱える |
| audio処理も含め全面IPC化 | 寿命・共有音声・event・GUI等まで広がる。scan問題に対して変更が過大 |

任意のinstalled pluginを製品から自動scanする現在の用途を重視するなら、二番目を独立設計課題として進めることを勧める。F1/F2の修正や、共通API全体のIPC化とは分離できる。ここでは未実装で、追加process基盤の導入を自動的に承認済みと扱わない。

## 11. 文書と型保証のずれ

構造を把握する際、READMEの強い断言をそのまま実装保証として扱わないことが重要だった。

- dependency-freeはbackend crate依存を防ぐが、public raw pointer、参照、callback、IPC不適合な型をRustのbuildが自動排除するわけではない。
- `SubPluginMain`がSendを要求しないことは、全実装の非Send性を強制することとは違う。
- `SubPluginProcessor: Send`は、process/resetが待機や確保をしないという証明ではない。
- params/snapshotのbatch化は境界の往復を減らすが、複数値がDSPと原子的に一致する意味ではない。
- CLAPを参考にする選択は妥当でも、片方の規格があらゆる概念で上位互換という関係ではない。値域・lifecycle・能力の違いは項目別に見る必要がある。
- ClassInfo/PluginRef等の「serialized」という説明は、そのRust型にserdeやwire protocolが実装されていることを意味しない。
- `catalogue::Class`のdocには、既に移動したmodule分類方針の説明が残る。これは実装責務が残っている証拠ではなく、古い説明の残存。

今回の解説文書はこのずれを補うが、既存README・code commentそのものは監査の一環として書き換えていない。

## 12. 検証結果と限界

| 検証 | 結果 |
| --- | --- |
| `plugin-host-api` unit | 14成功 |
| `plugin-host` unit | 2成功 |
| api Processor allocation/return integration | 1成功。fake DSP/handleの範囲 |
| host catalogue integration | 1成功 |
| host同梱CLAP facade integration | 1成功。load、metadata、editor、保存参照等 |
| 両crateの`cargo doc --no-deps` | 成功 |
| tempのEventSink容量probe | 1,000件で9回の確保・拡張を確認 |
| tempの構成不一致probe | block申告monoでもstereo native処理へ進むことを確認。十分なbacking sliceを渡して実領域外アクセスは回避 |

いずれもWindowsの現在のcheckoutで実行。temp probeは `temp/host-api-audit-probe/`、ソース取得記録は `temp/host-api-audit-sources/manifest.json`。既存の全面VST3 integration、実DAW、他OSのruntime、Miri/ASan、動的port変更をする実pluginの再現は今回実行していない。

既存テスト成功は「この共通APIの全契約を検証済み」という意味ではない。特に不正configの拒否、出力容量上限、any-thread通知、metadata変更、native規格の値域換算を横断する適合試験が不足している。今後の修正では、薄いmockの委譲確認だけでなく、これらの契約を両backendで満たす検証を追加すべき。

## 13. 修正する場合の順序

1. F1のsafe buffer境界。F2と独立して変更・検証しやすい。
2. F2のbounded event出力。超過時の音楽的な回復条件を含めて決める。
3. F3/F4の制御通知とmetadata更新。設計を一緒に検討し、配送条件と更新完了条件をコミット単位で区別する。
4. F5のnote観測とvoice回収方針。上位の挙動を明示してbackendから方針を移す。

規格固有の局所的な変換不一致は隣接backend監査で別途扱う。scanner隔離も独立の設計判断とし、上記修正の前提にはしない。今回依頼された作業は監査と解説までで、これらの実装には着手していない。
