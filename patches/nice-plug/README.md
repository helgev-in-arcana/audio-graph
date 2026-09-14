# 保留中のエディター表示要求パッチ

このブランチは実装案の保存用であり、mainへの採用は保留している。
nice-plugのforkリポジトリは作成しない。公式ソースへの差分とAudioGraph側の接続を、
既存audio-graphリポジトリのこのブランチだけに保存する。

`editor-open-requests.patch`はnice-plug 0.3.0の公式ソースに対する単一コミットで、
共通API、VST3/CLAPへの変換、主スレッド配送、取消、回帰テストを含む。
独立fork用に試作したCI workflowは含めていない。

- 公式ソース: https://codeberg.org/RustAudio/nice-plug
- 基点: `4450436d9f914aec780d17cea19f18d90fa863c9`
- 元のローカル修正コミット: `0eba0e05`
- 再適用後のGit tree: `820dd39cb11e62976ce9ba7ea6e43606ad21eb6f`（別checkoutで一致を確認済み）
- 原ライセンス: ISC。変更対象のライセンス表記はそのまま維持する。

## 再現手順

リポジトリ直下で実行する。`temp/external/nice-plug`が未作成のcheckoutを前提にする。
既存の作業がある場合は、そのディレクトリへ上書きせず、内容を確認する。

```sh
git clone --no-checkout https://codeberg.org/RustAudio/nice-plug.git temp/external/nice-plug
git -C temp/external/nice-plug checkout --detach 4450436d9f914aec780d17cea19f18d90fa863c9
git -C temp/external/nice-plug apply --check ../../../patches/nice-plug/editor-open-requests.patch
git -C temp/external/nice-plug apply ../../../patches/nice-plug/editor-open-requests.patch
cargo build --locked -p audio-graph-plugin
```

Cargo.tomlのpath patchが上記ディレクトリを参照するため、依存の取得とパッチ適用を省略した
checkoutはビルドできない。通常のCIも、この準備なしではそのまま実行できない。
mainへそのままマージできる依存構成としては扱わない。

```sh
cargo test --manifest-path temp/external/nice-plug/Cargo.toml -p nice-plug -p nice-plug-core --lib
cargo build --locked -p clap-test-plugin -p vst3-test-plugin
cargo test --locked -p audio-graph-plugin
```

AudioGraphの実機プラグイン選択を使うテストでは、`AUDIO_GRAPH_TEST_SUB`でビルドした
固定fixtureを明示する。Windowsでの検証では、`vst3_test_plugin.dll`を`.vst3`へコピーして指定した。

## 保存時の検証

- Windows: nice-plug/coreの74件、固定fixtureを使用したAudioGraphの66件が成功。
- nice-plug/coreのGUIなし構成のチェック、依存側の製品コードとAudioGraphのstrict Clippyが成功。
- nice-plug/coreのテスト込みstrict Clippyには、変更していないテストに4件の既存指摘がある。
- 実DAWでエディターが表示されることは未確認。

仕組みと制約は[通知設計の説明](../../docs/error-notifications.md)を参照。
採用を再検討する場合は、公式nice-plugのAPI対応状況から確認し、forkの維持を前提にしない。
