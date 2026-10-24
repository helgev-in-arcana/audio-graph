# vst3-test-plugin

VST3ホストの契約を確認する、別々のprocessor/controllerを持つ検証用plugin。
workspace buildでcdylibを作り、vst3-hostのfixture integration testからロードする。
製品からは依存しない。

実装はvst3 0.3.0のgain exampleを基に、値同期とnative資源の寿命を観測するhookを加えたもの。
元コードのライセンスは [LICENSE-MIT](LICENSE-MIT) / [LICENSE-APACHE](LICENSE-APACHE) に保持する。

パラメーターの値・段階数・processor/controller間の同期、バス構成、イベント容量、
editorの取得、COM参照の所有権と破棄順を、ホスト側のテストから確認するためのfixture。
アサーションは [vst3-host/tests](../vst3-host/tests) に置く。

リポジトリ直下から実行する:

```sh
cargo build --workspace --locked
cargo test -p vst3-host --locked
```

別パッケージのcdylibは `cargo test` だけでは生成されないため、先にビルドする。
インストール済みのVST3や `AUDIO_GRAPH_TEST_PLUGINS` の設定はfixture試験には不要。
Windowsでは、テストが `init_apartment()` のガードを保持してnative資源を扱う。
