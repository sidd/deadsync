#!/usr/bin/env bash
# Runs INSIDE an archlinux:base-devel container.
# Turns a deadsync release tarball into a pacman package plus a repo database.
#
# usage: build-pacman-repo.sh <tarball> <version> <pkgrel> <outdir>
set -euo pipefail

tarball="$(readlink -f "$1")"
ver="$2"
pkgrel="$3"
outdir="$(readlink -f "$4")"
src="$(cd "$(dirname "$0")/../.." && pwd)"

id builder >/dev/null 2>&1 || useradd -m builder
work=/home/builder/build
rm -rf "$work"
mkdir -p "$work" "$outdir"
cp "$src/packaging/arch/PKGBUILD" "$src/packaging/arch/deadsync-sidd.install" "$work/"
cp "$tarball" /home/builder/deadsync-linux.tar.gz
chown -R builder:builder /home/builder

su builder -c "cd $work && DEADSYNC_VER='$ver' SIDD_PKGREL='$pkgrel' SIDD_TARBALL=/home/builder/deadsync-linux.tar.gz makepkg -f --nodeps --nocheck --noconfirm"

pkg="$(ls "$work"/deadsync-sidd-*.pkg.tar.zst)"
cp "$pkg" "$outdir/"
cd "$outdir"
repo-add --new sidd.db.tar.zst "$(basename "$pkg")"
# GitHub release assets cannot be symlinks; pacman wants plain sidd.db / sidd.files.
cp --remove-destination sidd.db.tar.zst sidd.db
cp --remove-destination sidd.files.tar.zst sidd.files
ls -la "$outdir"
