#!/bin/sh
# Builds dist/ulap-linux_<ver>_<arch>.deb from target/release. Usage: packaging/deb.sh <version>
set -e
cd "$(dirname "$0")/.."
v=${1:?version}; v=${v#v}
arch=$(dpkg --print-architecture)
r=dist/deb/ulap-linux_${v}_$arch
rm -rf $r && mkdir -p $r/DEBIAN $r/usr/bin $r/usr/share/applications $r/usr/share/icons/hicolor/512x512/apps
install -m 755 target/release/ulap-linux target/release/ulap-account $r/usr/bin/
install -m 644 packaging/ulap.png $r/usr/share/icons/hicolor/512x512/apps/ulap.png
sed 's|^Exec=.*|Exec=/usr/bin/ulap-linux|' packaging/ulap.desktop > $r/usr/share/applications/ulap.desktop
cat > $r/DEBIAN/control <<EOT
Package: ulap-linux
Version: $v
Architecture: $arch
Maintainer: Ulap <noreply@example.com>
Depends: libc6
Section: utils
Priority: optional
Description: Ulap - private photo and video backup to Telegram
 Desktop port of the Ulap Android app.
EOT
dpkg-deb --root-owner-group --build $r dist/ulap-linux_${v}_$arch.deb
(cd dist && sha256sum ulap-linux_${v}_$arch.deb > ulap-linux_${v}_$arch.deb.sha256)
