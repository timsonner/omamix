# Maintainer: timsonner
pkgname=omamix
pkgver=0.1.0
pkgrel=1
pkgdesc='PipeWire live mixer that publishes one virtual microphone'
arch=('x86_64')
url='https://origin.cursor.com/timsonner/omamix'
license=('MIT')
depends=(
  'pipewire'
  'libpipewire'
  'gcc-libs'
  'glibc'
  'wayland'
  'libxkbcommon'
  'mesa'
)
makedepends=('git' 'cargo' 'clang' 'pkgconf' 'libpipewire')
# Arch repository packages only. Do not add AUR dependencies.
# libpipewire must be >= 1.2. The pipewire feature's build.rs rejects older headers.
# Dev stub: source is the Origin git repo. There is no release tarball yet.
options=('!lto' '!debug')

source=("git+https://origin.cursor.com/timsonner/omamix.git")
sha256sums=('SKIP')

pkgver() {
  cd "$srcdir/$pkgname"
  printf "0.1.0.r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
  cd "$srcdir/$pkgname"
  cargo fetch --locked --manifest-path Cargo.toml
}

build() {
  cd "$srcdir/$pkgname"
  cargo build --frozen --release \
    --manifest-path Cargo.toml \
    -p omamix --features pipewire \
    --target-dir "$srcdir/target"
}

package() {
  cd "$srcdir/$pkgname"
  install -Dm755 "$srcdir/target/release/omamix" "$pkgdir/usr/bin/omamix"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
