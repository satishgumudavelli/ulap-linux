#!/bin/sh
# Builds a release tarball in dist/. Usage: packaging/release.sh [version]
set -e
cd "$(dirname "$0")/.."
v=${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}
cargo build --release
n=ulap-linux-$v-$(uname -m)
rm -rf dist/$n && mkdir -p dist/$n
cp target/release/ulap-linux target/release/ulap-account packaging/ulap.png packaging/ulap.desktop packaging/install.sh dist/$n/
tar -C dist -czf dist/$n.tar.gz $n
(cd dist && sha256sum $n.tar.gz > $n.tar.gz.sha256)
echo "dist/$n.tar.gz"
