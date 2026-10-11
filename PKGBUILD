# Maintainer: 24bit192kHz <118996408+24bit192kHz@users.noreply.github.com>
pkgbase=hwinfo-rs
pkgname=(hwinfo-rs hwinfo-rs-dkms)
pkgver=0.1.0
pkgrel=1
pkgdesc="HWiNFO64-style hardware sensor monitor for Linux (GUI, CLI, Waybar)"
arch=(x86_64)
url="https://github.com/24bit192kHz/hwinfo-rs"
license=(GPL-3.0-or-later)
makedepends=(cargo git)
source=("git+$url.git")
sha256sums=(SKIP)

pkgver() {
  cd "$pkgbase"
  printf "0.1.0.r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
  cd "$pkgbase"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$pkgbase"
  export RUSTUP_TOOLCHAIN=stable CARGO_TARGET_DIR=target
  cargo build --frozen --release
}

package_hwinfo-rs() {
  depends=(lm_sensors libxkbcommon wayland libx11)
  optdepends=(
    'hwinfo-rs-dkms: VRAM/hot-spot, Zen IOD temps and timings, DIMM/VRM sensors'
    'nvidia-utils: NVIDIA GPU sensors (NVML/NVAPI)'
    'smartmontools: S.M.A.R.T. data in the root helper'
    'ryzen_smu-dkms-git: AMD SMU power table (per-core power, SVI2, PPT/TDC/EDC)'
    'dmidecode: memory module details in the summary'
    'libnotify: desktop notifications for alerts'
    'ttf-ms-fonts: Segoe UI for the exact HWiNFO look'
  )
  cd "$pkgbase"
  install -Dm755 target/release/hwinfo-rs "$pkgdir/usr/bin/hwinfo-rs"
  install -Dm644 dist/hwinfo-rs-helper.service "$pkgdir/usr/lib/systemd/system/hwinfo-rs-helper.service"
  install -Dm644 dist/hwinfo-rs.modules-load.conf "$pkgdir/usr/lib/modules-load.d/hwinfo-rs.conf"
  install -Dm644 dist/90-hwinfo-rs-jc42.rules "$pkgdir/usr/lib/udev/rules.d/90-hwinfo-rs-jc42.rules"
  install -Dm644 dist/hwinfo-rs.desktop "$pkgdir/usr/share/applications/hwinfo-rs.desktop"
  install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm644 analysis/RE-REPORT.md "$pkgdir/usr/share/doc/$pkgname/RE-REPORT.md"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}

package_hwinfo-rs-dkms() {
  pkgdesc="Kernel modules for hwinfo-rs: NVIDIA VRAM/hot-spot, Zen 2/3 IOD temps + timings, Gigabyte B550 SMBus sensors"
  license=(GPL-2.0-only)
  depends=(dkms)
  cd "$pkgbase/kmod"
  local m ver
  for m in nvvram zen_extras gsa1_smbus; do
    ver=$(sed -n 's/^PACKAGE_VERSION="\(.*\)"/\1/p' "$m/dkms.conf")
    name=$(sed -n 's/^PACKAGE_NAME="\(.*\)"/\1/p' "$m/dkms.conf")
    install -Dm644 -t "$pkgdir/usr/src/$name-$ver" "$m"/{Makefile,dkms.conf,*.c}
  done
  install -Dm644 ../LICENSES/GPL-2.0-only.txt "$pkgdir/usr/share/licenses/$pkgname/GPL-2.0-only.txt"
}
