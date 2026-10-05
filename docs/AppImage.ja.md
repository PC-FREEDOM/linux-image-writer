# Linux Image Writer の AppImage

[English](AppImage.md)

v0.1.1 から、Linux Image Writer の各リリース(GitHub Release)に AppImage
が付きます。インストールせずに、多くの Linux ディストリビューションで
そのまま動く1つのファイルです。

## ダウンロードと確認

リリースのページから、次の2つのファイルをダウンロードします(v0.1.2 の
例):

- `LinuxImageWriter-0.1.2-x86_64.AppImage`
- `LinuxImageWriter-0.1.2-x86_64.AppImage.sha256`

保存したフォルダで、ダウンロードしたファイルを確認します。

```sh
sha256sum -c LinuxImageWriter-0.1.2-x86_64.AppImage.sha256
```

`LinuxImageWriter-0.1.2-x86_64.AppImage: OK` と表示されれば正常です。
そう表示されない場合は、ファイルを削除してダウンロードし直してください。

## 起動

```sh
chmod +x LinuxImageWriter-0.1.2-x86_64.AppImage
./LinuxImageWriter-0.1.2-x86_64.AppImage
```

ファイルマネージャーで実行を許可して(プロパティ → アクセス権)、そこから
開くこともできます。

アプリが root で動くことはなく、起動時にパスワードを求めることもありません。
USB ドライブへの書き込みは UDisks2 を通して行い、そのときにデスクトップの
polkit エージェントが認証を求めることがあります。

### USB ドライブなどのリムーバブルメディアから起動できない場合

FAT でフォーマットされた USB ドライブなどのリムーバブルメディアでは、
デスクトップのマウント設定によって、その上のプログラムを直接実行できない
ことがあります(ファイルに実行権限を付けられないなど)。その場合は、
AppImage をホームフォルダや `~/Downloads` などのローカルのフォルダへ
コピーしてから起動してください。

### FUSE が使えない場合

AppImage は FUSE で自分自身をマウントして動きます。それができない環境
(`/dev/fuse` のないコンテナなど)では、次のように起動します。

```sh
./LinuxImageWriter-0.1.2-x86_64.AppImage --appimage-extract-and-run
```

この場合は、いったん一時フォルダに展開してから起動します(約 95 MB。
アプリの終了後も `/tmp` に残ることがあります)。

## 動作環境

- x86_64 の Linux で、glibc 2.39 以降(Ubuntu 24.04、Debian 13、Fedora 40
  以降、openSUSE Tumbleweed など)
- FUSE 3(`fusermount3`。多くのディストリビューションでは `fuse3`
  パッケージ)。または上の `--appimage-extract-and-run`
- UDisks2、D-Bus のシステムバスとセッションバス、polkit の認証エージェント
  (通常はデスクトップ環境に含まれています)
- `shared-mime-info`(デスクトップ環境には必ず入っています)。ないと
  アプリのアイコンを表示できません
- 表示する言語に対応したフォント。日本語の場合は CJK に対応したフォント
  (Noto Sans CJK など)。AppImage はシステムのフォントを使います
- 推奨: `xdg-desktop-portal`(デスクトップのファイル選択ダイアログを
  使うため)

Wayland と X11 のどちらでも、また GNOME、KDE Plasma、Xfce などの
デスクトップで動きます。GTK 4 と libadwaita は AppImage に含まれ、
グラフィックスドライバ(Mesa など)はシステムのものをそのまま使います。

## ライセンスとソースコード

- Linux Image Writer は GNU General Public License version 3 以降
  (GPL-3.0-or-later)で提供されています。
- AppImage に含まれるすべてのもののライセンスは、AppImage の中の
  `usr/share/doc/` にあります。アプリの `LICENSE`、アプリに組み込まれた
  Rust クレートのライセンス(`linux-image-writer/THIRD-PARTY-LICENSES.txt`)、
  同梱した Ubuntu のライブラリの各パッケージの copyright です。読むには、
  `--appimage-extract` で AppImage を展開してください。
- 各リリースには、2つのソースアーカイブが付きます。
  - `LinuxImageWriter-<version>-source.tar.gz`: Linux Image Writer 自身の
    ソースコード。そのリリースのタグの内容そのものです。
  - `LinuxImageWriter-<version>-appimage-sources.tar.zst`: 同梱した
    ライブラリのうち、ライセンス(GPL、LGPL、MPL など)が求めるものの
    対応ソースコード。Ubuntu が公開したものそのままです。多くの利用者には
    必要ありません。
