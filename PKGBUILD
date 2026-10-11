# Maintainer: timsonner
# The git source is unpinned: sha256sums is SKIP and pkgver() follows
# whatever commit HEAD is when makepkg runs. The Origin git URL is not
# anonymous. A clean chroot clone needs credentials.
# `makepkg --nobuild` failed here without them.
# The omamix binary does not call pactl. libpulse is not a dependency.
#
# Runtime NEEDED (readelf -d on the pipewire-feature binary):
#   libpipewire-0.3.so.0, libgcc_s.so.1, libm.so.6, libc.so.6,
#   ld-linux-x86-64.so.2.
# libpipewire is that runtime library. It is not also a makedepend:
# Arch installs depends before the build.
# glibc covers libc, libm, and the dynamic linker.
# libgcc_s is implicit through the toolchain. gcc-libs and libgcc are omitted.
# The pipewire daemon package is not a library the binary loads.
# eframe dlopens Wayland, xkbcommon, and EGL even though readelf NEEDED
# omits them, so those packages stay:
#   wayland       libwayland-client.so.0 and libwayland-egl.so.1
#   libxkbcommon  libxkbcommon.so.0
#   mesa          libEGL.so.1
# wgpu also dlopens libvulkan.so.1 when a loader is present. That loader
# is not a package dependency.
pkgname=omamix-git
pkgver=0.1.0
pkgrel=1
pkgdesc='PipeWire live mixer that publishes one virtual microphone'
arch=('x86_64')
url='https://origin.cursor.com/timsonner/omamix'
license=('MIT')
depends=(
  'libpipewire'
  'glibc'
  'wayland'
  'libxkbcommon'
  'mesa'
)
makedepends=('git' 'cargo' 'clang' 'pkgconf')
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
