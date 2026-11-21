# Ulap for Linux

Desktop port of the Ulap Android app: private photo/video backup to your own Telegram chat, using Telegram bots.
Rust + egui. Compatible with the Android app's chunked-file layout and pinned index.

## Features
- Gallery of everything in your Telegram chat, with thumbnails; delete, restore, back up again
- Bulk upload with 429 handling, retries and a conservative mode; multiple bots
- Drag and drop files (photos, videos, PDF, mobi; "don't split" for non-media)
- Multiple chats; create a chat via a guided wizard or by Telegram account login
- Optional "Strip location" (removes GPS from JPEGs before upload, off by default)

## Install (prebuilt)
**Windows:** download the `-windows-x86_64.zip` from Releases, unzip, run `ulap-linux.exe` (keep `ulap-account.exe` beside it). Unsigned, so SmartScreen may warn.

**Debian/Ubuntu:** download the `.deb` from Releases, then `sudo apt install ./ulap-linux_*.deb`.

**Other Linux:** download the tarball from Releases, then:

    tar xzf ulap-linux-*.tar.gz && cd ulap-linux-*/ && ./install.sh

## Build from source
Needs Rust (stable).

    cargo build --release
    install -m 755 target/release/ulap-linux target/release/ulap-account ~/.local/bin/

Make a release tarball: `packaging/release.sh` (output in `dist/`).

## Data
Settings (bot tokens): `~/.config/ulap/config.json` (mode 0600). Data: `~/.local/share/ulap/`. Neither is part of this repo.

## Notes
- Files over 20 MB uploaded unsplit can't be downloaded by a bot; the gallery opens them in Telegram.
- Android's index rewrite drops the non-media `files` list.
