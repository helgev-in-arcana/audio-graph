# Roadmap

## Planned Features / 予定

- macOS 対応 (ウィンドウ実装がまだありません)
- Linux の HiDPI (`Xft.dpi` が無い環境では等倍に落ちます)
- プラグイン読み込み制限を無くしたい
- 出力バスを 1 本も宣言しないプラグインが、オーディオのコンパイル対象から外れることの修正。

## Pending Features　/ 保留

- Wayland ネイティブ対応。VST3 には API がありますが CLAP は渡すハンドルが未定義で、
  エディタが載っている baseview にも Wayland バックエンドがありません。現状 XWayland で
  動くので、無理に対応を試みるメリットが無い。

## Limitations / 今後も出来ないこと

- VST2ネイティブ対応
  - ライセンスがありません
- VST3のAudioGraphでの可変数オートメーションスロット
  - VST3規格がこれに対応しません

## Known Issues / 既知のバグ

見つけた場合は[Issue](https://github.com/helgev-in-arcana/audio-graph/issues)まで。
一方で、現在アルファ版ですので開発によってバグが直ったり増えたり復活したりするかもしれません。
