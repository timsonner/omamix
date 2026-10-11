# Maintainer: timsonner
# The git source is unpinned: sha256sums is SKIP and pkgver() follows
# whatever commit HEAD is when makepkg runs. The Origin git URL is not
# anonymous. A clean chroot clone needs credentials.
# `makepkg --nobuild` failed here without them.
# The omamix binary does not call pactl. libpulse is not a dependency.
pkgname=omamix-git
pkgver=0.1.0
pkgrel=1
pkgdesc='PipeWire live mixer that publishes one virtual microphone'
arch=('x86_64')
url='https://origin.cursor.com/timsonner/omamix'
license=('MIT')
depends=(
  'pipewire'
  'libgcc'
  'glibc'
  'wayland'
  'libxkbcommon'
  'mesa'
)
# libpipewire is the build dependency. The pipewire package already depends
# on it at runtime, so it is not listed twice.
makedepends=('git' 'cargo' 'clang' 'pkgconf' 'libpipewire')
provides=('omamix')
conflicts=('omamix')
# Arch repository packages only. Do not add AUR dependencies.
# libpipewire must be >= 1.2. The pipewire feature's build.rs rejects older headers.
options=('!lto' '!debug')

source=("omamix::git+https://origin.cursor.com/timsonner/omamix.git")
sha256sums=('SKIP')

pkgver() {
  cd "$srcdir/omamix"
  printf "0.1.0.r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
  cd "$srcdir/omamix"
  cargo fetch --locked --manifest-path Cargo.toml
}

build() {
  cd "$srcdir/omamix"
  cargo build --frozen --release \
    --manifest-path Cargo.toml \
    -p omamix --features pipewire \
    --target-dir "$srcdir/target"
}

check() {
  cd "$srcdir/omamix"
  cargo test --frozen --workspace
}

package() {
  cd "$srcdir/omamix"
  install -Dm755 "$srcdir/target/release/omamix" "$pkgdir/usr/bin/omamix"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
