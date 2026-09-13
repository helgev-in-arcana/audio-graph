# vst3-test-plugin

VST3ホストの契約を確認する、別々のprocessor/controllerを持つ検証用plugin。
workspace buildでcdylibを作り、vst3-hostのfixture integration testからロードする。
製品からは依存しない。

実装はvst3 0.3.0のgain exampleを基に、値同期とnative資源の寿命を観測するhookを加えたもの。
元コードのライセンスはLICENSE-MIT / LICENSE-APACHEに保持する。
