pkgname=omamix-git
pkgver=0.1.0
pkgrel=1
pkgdesc='PipeWire live mixer that publishes one virtual microphone'
arch=('x86_64' 'aarch64')
url='https://github.com/timsonner/omamix'
license=('MIT')
depends=(
  'pipewire'
  'libpipewire'
  'glibc'
  'wayland'
  'libxkbcommon'
  'mesa'
)
makedepends=('git' 'cargo' 'clang' 'pkgconf')
provides=('omamix')
conflicts=('omamix')
options=('!lto' '!debug')

source=("omamix::git+https://github.com/timsonner/omamix.git")
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
  install -Dm644 packaging/omamix.desktop "$pkgdir/usr/share/applications/omamix.desktop"
  install -Dm644 packaging/omamix.service "$pkgdir/usr/lib/systemd/user/omamix.service"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
