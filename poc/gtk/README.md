# GTK4 / libadwaita 配布 PoC(Phase 3B-0A)

Linux USB Writer の GUI toolkit として GTK4 と libadwaita を採用してよいかを、配布方法(Flatpak / AppImage)と UDisks2 / polkit との組み合わせまで含めて確かめた PoC です。

**結論: GTK4 + libadwaita を採用しました(ADOPT)。Phase 3B-0A は完了しています。**

この PoC は証跡として残しているもので、本番の GUI ではありません。

## 構成

- このディレクトリは独立した Cargo package です。workspace のメンバーではありません。
  - Production の library を `path = "../.."` で参照しています。
  - Production の CLI と library のビルドに GTK は入りません。
  - このディレクトリを削除するだけで、PoC を取り除けます。
- `gtk-poc`(`src/main.rs`)が使う Production API は、読み取り専用の `list_candidates()` と、`DeviceCandidate` の読み取り用メソッドだけです。
  - 書き込み要求、worker、デバイスの open は使いません。
  - シリアル番号は表示も出力もしません。

## 実行モード

| コマンド | 内容 |
| --- | --- |
| `gtk-poc` | 画面を表示します。<br>表示するもの: GTK / libadwaita のバージョン、sandbox のチェック結果、表示バックエンドと配色(同じ内容を stdout にも出力します)、image の選択ボタン(FileChooser portal)、デバイス一覧。 |
| `gtk-poc --probe` | デバイス一覧と、sandbox のチェック結果(`/proc/swaps`、`/sys/block/*/diskseq`、sandbox の中かどうか)をテキストで出力します。ディスプレイは不要です。 |
| `gtk-poc --source-identity <path> <秒>` | ファイルを開き、Production の Source Identity が比べる値を fstat で 2 回取って比べます。比べる値は dev / ino / size / mtime / ctime です。 |
| `gtk-poc --source-identity-poll <path> <秒>` | 同じ値を 50 ms ごとに取り、変更が見えた時刻を出力します。 |

ネイティブでは `cargo run --release -- <引数>` で実行します。

## Flatpak

manifest は `io.github.pcfreedom.LinuxUsbWriter.GtkPoc.yml` です。Flathub への提出用ではありません。

- **runtime**: org.gnome.Platform 50。SDK は org.gnome.Sdk 50 と rust-stable を使います。
- **権限**: `--share=ipc`、`--socket=wayland`、`--socket=fallback-x11`、`--device=dri`、`--system-talk-name=org.freedesktop.UDisks2` だけです。
  - `--device=all`、`--socket=system-bus`、`--filesystem` は使っていません。
- **ビルド時のネットワーク**: ビルドの間だけ、cargo が crate を取得するためにネットワークを使います。Flathub では `flatpak-cargo-generator` によるオフラインのソースが必要です。
- **同梱物**: 同じ sandbox に、変更していない Production CLI(`linux-usb-writer`)も入れています。

ビルド、インストール(ユーザー領域)、実行、削除:

```sh
flatpak run org.flatpak.Builder --force-clean --repo=repo build-dir io.github.pcfreedom.LinuxUsbWriter.GtkPoc.yml
flatpak build-bundle repo gtk-poc.flatpak io.github.pcfreedom.LinuxUsbWriter.GtkPoc
flatpak --user install --bundle gtk-poc.flatpak
flatpak run io.github.pcfreedom.LinuxUsbWriter.GtkPoc
flatpak run io.github.pcfreedom.LinuxUsbWriter.GtkPoc --probe
flatpak --user uninstall io.github.pcfreedom.LinuxUsbWriter.GtkPoc
```

### Final Validation の再現方法

**1. 正規の `flatpak run`**
- `--probe` を実行し、画面を起動します。
- system bus で UDisks2 以外の名前が遮断されていることを確かめます。

  ```sh
  flatpak run --command=gdbus <ID> call --system --dest org.freedesktop.login1 ...
  ```

  `ServiceUnknown` になれば遮断されています。

**2. OpenDevice、polkit、FD の受け取り**
- 次のコマンドを実行し、表示された polkit の認証ダイアログで**ユーザー本人が**認証します。

  ```sh
  flatpak run --command=linux-usb-writer io.github.pcfreedom.LinuxUsbWriter.GtkPoc \
      open-test /org/freedesktop/UDisks2/block_devices/<dev>
  ```

- `open-test` は Production CLI に元からある診断モードです。処理の流れは次のとおりです。
  1. 選択と Safety の再検証
  2. OpenDevice(`rw`、O_EXCL)
  3. FD の major:minor、`BLKGETSIZE64`、`BLKGETDISKSEQ` を期待値と照合(FD binding)
  4. **何も書き込まずに(0 bytes)** FD を閉じる
- 出力の最後が次の 3 行なら PASS です。

  ```
  FD binding: Match
  Write performed: NO (0 bytes)
  FD closed: yes
  ```

**3. portal 経由の image と Source Identity**
- ホスト側で、ファイルを document portal に登録します。

  ```sh
  flatpak document-export --app=<ID> --allow-read <file>
  ```

  このとき表示される `/run/user/<uid>/doc/<id>/<name>` が、sandbox の中から見えるパスです。FileChooser portal で選んだときと同じ仕組みです。
- sandbox の中で `--source-identity <そのパス> 5` を実行し、待っている間にホスト側でファイルを変更します。
  - 変更の例: 追記、`touch`、`chmod`、同じサイズでの上書き、rename による置き換え
- `result: CHANGED` になることを確認します。

### 結果(2026-09-27、KDE Plasma / Wayland)

| 項目 | 結果 |
| --- | --- |
| 正規の `flatpak run` | PASS。Wayland で表示され、portal の配色(ダーク)が反映された。一覧と Safety の判定はネイティブと同じ。diskseq と `/proc/swaps` も読めた。 |
| OpenDevice、polkit、FD | PASS。ホストの polkit agent で認証できた。FD は sandbox の中に届いた。major:minor、size、diskseq がすべて一致し、`/proc/self/fd` は `/dev/sda` を指していた。FD binding: Match。書き込みは 0 bytes。 |
| portal と Source Identity | PASS。dev / ino は portal 独自の値だが、同じ FD では変わらない。size / mtime / ctime はナノ秒単位でホストと一致した。変更はすべて検出でき、見えるまでの遅れはネイティブと同じだった。 |

## AppImage

### quick-sharun(Anylinux)【現時点での第一候補】

`pkgforge-dev/Anylinux-AppImages` の `useful-tools/quick-sharun.sh` を使う方式です。
- glibc まで同梱するので、ホストの glibc に依存しません。
- FUSE がない環境でも、展開して起動します。
- この 2 点は、glibc 2.42 の環境(Freedesktop 25.08 の runtime の中)で確認しました。

注意点:
- 手元の PC でビルドすると、その PC の GPU ドライバ(NVIDIA のプロプライエタリなものを含む)まで同梱され、サイズも大きくなります。**配布用はクリーンな CI コンテナでビルドしてください。**
- quick-sharun は、ビルド中にアプリを起動して必要なライブラリを調べます。そのときに `dbus-launch` で起動した dbus-daemon などが残ることがありました。ビルドの後は、残っているプロセスを確認してください。

### linuxdeploy + GTK plugin【標準方式としては採用しない】

`./build-appimage.sh <tools-dir>` で作れます。PoC の証跡として残しています。

採用しない理由:
- GTK plugin は、2023-10 以降ほとんど保守されていません。
- Wayland の環境でも `GDK_BACKEND=x11` を強制します。
- `GTK_THEME` を強制するので、libadwaita の見た目が崩れます。
- ビルドした PC の glibc を要求します。今回の Arch 系の PC では GLIBC_2.43 が必要になりました。

使ったツール(システムにはインストールせず、一時ディレクトリで使用):

| ツール | 取得元とバージョン |
| --- | --- |
| linuxdeploy | continuous(07333c6) |
| linuxdeploy-plugin-gtk.sh | 3b67a1d |
| appimagetool | continuous(8c8c91f) |
| patchelf | 0.19.1 |

生成物(`target/`、`build-dir/`、`.flatpak-builder/`、`repo/`、`appimage/`、`*.AppImage`、`*.flatpak`)は `.gitignore` で除外しています。
