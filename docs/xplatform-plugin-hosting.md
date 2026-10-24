# XPlatform Plugin Hosting Library / プラグインホストライブラリ

[`plugin-host`](../crates/plugin-host/README.md) は、RustからVST3 / CLAPを読み込み、
パラメーター・音声・MIDI・状態・エディターを共通APIで扱うためのライブラリです。
`Plugin::load` はパスの拡張子から形式を選ぶので、通常の処理側に形式ごとの分岐は不要です。

APIはAudioGraph内部での利用が中心で、まだ安定していません。
実装上の契約は [plugin-host README](../crates/plugin-host/README.md) と
[plugin-host-api README](../crates/plugin-host-api/README.md) を正本とし、この文書では利用の入口を説明します。

## 使うクレート

| 用途 | クレート |
| --- | --- |
| 分かっているパスからロードし、処理する | `plugin-host`。共通の型とtraitも再公開する。 |
| フォルダーを列挙し、カタログから保存参照を解決する | [plugin-scan](../crates/plugin-scan/README.md)。探索先は呼び出し側が渡す。 |
| 別のプラグインの中でホストし、オートメーション・状態・transportをつなぐ | [subhost-adapter](../crates/subhost-adapter/README.md)。 |
| AudioGraphのフォルダー設定やピン留めを共有する | [audio-graph-settings](../crates/audio-graph-settings/README.md)。一般のホストに必須ではない。 |

スキャンは第三者のコードをロードするため、UIスレッド外で行います。
`plugin-scan::reference_candidates` は既存のカタログから候補を返すだけで、ロードやディスク探索は行いません。
実際のロードでは保存したクラスIDを指定して、その候補が目的のプラグインか確認します。

## ロード・処理・停止

以下は、同じスレッドでロードから1ブロックの処理、停止までを行うライフサイクルの例です。
ホストのサービスを実装した `HostContext`、対応するバス構成、音声バッファ、
入力イベント、事前確保した出力sinkを呼び出し側が渡します。

```rust
use std::{path::Path, sync::Arc};

use plugin_host::{
    AudioBuffers, AudioConfig, Event, EventSink, HostContext, Plugin,
    ProcessStatus, Result, SubPluginMain, SubPluginProcessor, TimeContext,
};

pub fn process_one_block(
    path: &Path,
    host_context: Arc<dyn HostContext>,
    config: AudioConfig,
    buffers: &mut AudioBuffers<'_>,
    events: &[Event],
    time: &TimeContext,
    out_events: &mut EventSink,
) -> Result<(ProcessStatus, bool)> {
    let _thread = plugin_host::init_thread()?;
    let mut plugin = Plugin::load(path, None, host_context)?;
    let mut processor = plugin.activate(config)?;

    out_events.clear();
    let status = processor.process(buffers, events, time, out_events);
    let output_overflowed = out_events.overflowed();

    processor.deactivate();
    plugin.tick();
    drop(plugin);
    plugin_host::reclaim_main_thread();

    Ok((status, output_overflowed))
}
```

戻り値の `ProcessStatus` は `Continue` / `Silent` / `Error`、boolは出力イベントの容量超過です。
呼び出し側は両方を確認し、エラーや容量超過のあるイベント列を完全な処理結果として扱わないでください。
ロード等の `Result` が成功したことと、音声処理が正常だったことは別です。

継続処理ではロード・バッファ確保・activateを準備段階に置き、processorを保持して
`process` だけをブロックごとに呼びます。この関数全体をオーディオコールバックから呼ぶ構成にはしません。
`Plugin::load` の第2引数が `None` なら先頭クラスを選びます。保存データの復元時はクラスIDを渡します。

## 処理前に決めるもの

- **音声構成:** `io_layout()` を読み、必要なら非稼働中に `request_main_bus_channels()` で主バス幅を要求します。
  `AudioConfig` のチャンネル数・補助バス・最大ブロック長に合うplanar形式のバッファを用意します。
  入出力はそれぞれ平坦な領域で、主バスの後に補助バスを並べます。
- **入力イベント:** `AudioConfig::max_input_events` に上限を設定します。
  各ブロックのイベントはサンプルオフセット順で、そのブロック内に収まる必要があります。
  バックエンドは容量や時刻が不正な入力を部分的に配送せず拒否します。
- **出力イベント:** オーディオスレッド外で `EventSink::with_capacity` を用意します。
  容量は実行中に増えません。収集区間の開始時に `clear()` し、処理後に `overflowed()` を確認します。
  複数の処理区間を合流させる場合、イベント時刻の付け替えと容量超過からの復旧は呼び出し側が担当します。

処理エラーから再開する場合は、processorのリセットと呼び出し側のノート追跡状態の復旧を行います。
バッファ形状、パラメーター値、メタデータ更新の契約は
[Processing and metadata contracts](../crates/plugin-host-api/README.md#processing-and-metadata-contracts) を参照してください。

## スレッド、コールバック、終了順

`Plugin` のメイン側操作は生成元スレッドに置き、`activate()` が返す所有権付きの
`Processor` を音声処理側へ渡します。初期化ガードは、そのスレッドのプラグイン・ビュー・processorの
ネイティブ資源をすべて解放するまで保持します。

メイン側のループでは次の処理を行います。

1. `plugin.tick()` をエディターの開閉によらず呼び、コールバック、タイマー、再構成要求を進めます。
2. `poll()` でライブラリ所有のウィンドウイベントを進めます。
   自分のメッセージキューを持つ単体ホストは `pump_events()` を使えますが、
   DAW内のプラグインはDAWのキューを代わりにdispatchしません。
3. `reclaim_main_thread()` で別スレッドから返却された資源を回収します。

`refresh_metadata()` が `NeedsDeactivation` を返した場合はprocessorを停止・回収してから再試行し、
更新された記述に合わせてバッファと構成を準備し直します。
`HostContext` のコールバックでは同じプラグインへ再入せず、処理すべき要求を記録します。

終了時は音声処理を止めてprocessorを返却し、所有スレッドで回収します。
残る通知をtickで処理し、エディターとプラグインを解放してから初期化ガードを破棄します。
別スレッドでの `deactivate()` やdropはネイティブ資源の即時破棄を意味しません。

## エディターと実装例

子プラグインのエディター用コンテナはWin32とX11で実装されています。
LinuxのWaylandセッションではXWaylandが必要で、macOSのコンテナ生成は未実装です。
詳細は [host-windowのBackends](../crates/host-window/README.md#backends) を参照してください。

実際の `HostContext` は [host-cli/src/host.rs](../crates/host-cli/src/host.rs)、
ロード・オフライン処理・GUIの実装例は [host-cli/src/main.rs](../crates/host-cli/src/main.rs) にあります。
[host-cli README](../crates/host-cli/README.md) から、目的に合う検証コマンドを選べます。
