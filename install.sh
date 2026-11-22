#!/bin/sh
# Install cl from the latest GitHub release:
#
#   curl -fsSL https://github.com/camhpj/copy-last/releases/latest/download/install.sh | sh
#
# Environment variables:
#   CL_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   CL_VERSION       a release tag such as v0.1.0 (default: the latest release)

set -eu

REPO="camhpj/copy-last"

main() {
  install_dir=${CL_INSTALL_DIR:-$HOME/.local/bin}
  version=${CL_VERSION:-latest}
  target=$(detect_target)

  if [ "$version" = latest ]; then
    base="https://github.com/$REPO/releases/latest/download"
  else
    base="https://github.com/$REPO/releases/download/$version"
  fi
  archive="cl-$target.tar.gz"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  say "downloading $archive ($version)"
  download "$base/$archive" "$tmp/$archive"
  download "$base/$archive.sha256" "$tmp/$archive.sha256"
  verify "$tmp" "$archive"

  tar -xzf "$tmp/$archive" -C "$tmp"
  mkdir -p "$install_dir"
  install -m 755 "$tmp/cl-$target/cl" "$install_dir/cl"
  say "installed $("$install_dir/cl" --version) to $install_dir/cl"

  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) say "$install_dir is not on your PATH. Add it in your rc file:"
       say "  export PATH=\"$install_dir:\$PATH\"" ;;
  esac
  say "next, add the hook to your rc file, then run \`cl shell\`:"
  say "  eval \"\$(cl init zsh)\"   # or: eval \"\$(cl init bash)\""
}

detect_target() {
  case $(uname -s) in
    Darwin) os=apple-darwin ;;
    Linux) os=unknown-linux-musl ;;
    *) fail "cl supports macOS and Linux, not $(uname -s)" ;;
  esac
  case $(uname -m) in
    x86_64 | amd64) arch=x86_64 ;;
    arm64 | aarch64) arch=aarch64 ;;
    *) fail "no cl build for $(uname -m)" ;;
  esac
  echo "$arch-$os"
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1" || fail "cannot download $1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q --https-only -O "$2" "$1" || fail "cannot download $1"
  else
    fail "this script needs curl or wget"
  fi
}

verify() {
  expected=$(cut -d ' ' -f 1 "$1/$2.sha256")
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$1/$2" | cut -d ' ' -f 1)
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$1/$2" | cut -d ' ' -f 1)
  else
    fail "this script needs sha256sum or shasum to check the download"
  fi
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $2"
}

say() {
  printf 'cl-install: %s\n' "$1"
}

fail() {
  printf 'cl-install: error: %s\n' "$1" >&2
  exit 1
}

# Everything runs from here, so a partly downloaded script does nothing.
main "$@"
