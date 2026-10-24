# Roadmap

## Planned Features / 予定

- macOS 対応（ウィンドウ実装がまだ）
- Linux の HiDPI（`Xft.dpi` が無い環境でUIスケーリングが未対応）
- プラグイン読み込み数制限の撤廃（現在は1パッチ最大16個）
- 出力バスを1本も宣言しないプラグインが、オーディオのコンパイル対象から外れることの修正

## Pending Features / 保留

- Wayland ネイティブ対応。VST3 には API がありますが CLAP は渡すハンドルが未定義で、
  エディタが載っている baseview にも Wayland バックエンドがありません。現状 XWayland で
  動くので、無理に対応を試みるメリットが無い。

## Current Alpha Limits & Specifications / 現在の上限・仕様

- 1パッチ内のサブプラグインは最大 **16個**、DAWへ公開するオートメーションスロットは **32個** 固定です。
  - VST3 規格が動的なパラメーター数の増減に対応しないため、スロット数は固定としています。
- VST2 ネイティブ対応はライセンスの都合上行いません（VST3 ラッパー等を利用してください）。

## Known Issues / 既知のバグ

見つけた場合は [Issue](https://github.com/helgev-in-arcana/audio-graph/issues) まで。
現在アルファ版ですので、開発によってバグが直ったり増えたり復活したりするかもしれません。
