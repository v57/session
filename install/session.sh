#!/usr/bin/env bash
# Installs the latest `session` release on macOS or Linux:
#
#   curl -fsSL https://v57.dev/install/session | bash
#
# Options (environment variables):
#   SESSION_VERSION      a release tag such as v0.1.0 (default: the latest release)
#   SESSION_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
#   SESSION_DOWNLOAD_URL where the release assets live (for mirrors and testing)
#
# The whole script is one function called on the last line, so a download cut
# short never runs half of it.

set -euo pipefail

main() {
  local repo="v57/session"
  local version="${SESSION_VERSION:-latest}"
  local bin_dir="${SESSION_INSTALL_DIR:-$HOME/.local/bin}"

  local os arch
  case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux) os="unknown-linux-musl" ;;
    *) die "unsupported system: $(uname -s). On Windows, run: powershell -c \"irm v57.dev/install/session.ps1|iex\"" ;;
  esac
  case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    arm64 | aarch64) arch="aarch64" ;;
    *) die "unsupported CPU: $(uname -m)" ;;
  esac
  # A shell running under Rosetta reports x86_64 on Apple silicon: install the native build.
  if [ "$os" = "apple-darwin" ] && [ "$arch" = "x86_64" ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = "1" ]; then
    arch="aarch64"
  fi

  local asset="session-$arch-$os.tar.gz"
  local base
  if [ -n "${SESSION_DOWNLOAD_URL:-}" ]; then
    base="$SESSION_DOWNLOAD_URL"
  elif [ "$version" = "latest" ]; then
    base="https://github.com/$repo/releases/latest/download"
  else
    base="https://github.com/$repo/releases/download/$version"
  fi

  # Global, not local: the EXIT trap runs after main has returned.
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  echo "Downloading $asset ($version)"
  download "$base/$asset" "$tmp/$asset"
  download "$base/SHA256SUMS" "$tmp/SHA256SUMS"

  local expected actual
  expected="$(awk -v f="$asset" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")"
  [ -n "$expected" ] || die "SHA256SUMS has no entry for $asset"
  actual="$(sha256 "$tmp/$asset")"
  [ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"
  echo "Checksum OK"

  tar -xzf "$tmp/$asset" -C "$tmp" session
  mkdir -p "$bin_dir"
  # Replace via rename so a running daemon keeps its (old) binary intact.
  cp "$tmp/session" "$bin_dir/.session.new"
  chmod 755 "$bin_dir/.session.new"
  mv -f "$bin_dir/.session.new" "$bin_dir/session"
  echo "Installed $("$bin_dir/session" --version) to $bin_dir/session"

  if pgrep -f "session --daemon" >/dev/null 2>&1; then
    echo
    echo "A session daemon is already running and keeps its version until it exits."
    echo "If session reports a version mismatch, end your sessions and run: pkill -f 'session --daemon'"
  fi

  case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) path_hint "$bin_dir" ;;
  esac
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 3 -o "$2" "$1" || die "download failed: $1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1" || die "download failed: $1"
  else
    die "curl or wget is required"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{ print $1 }'
  else
    die "sha256sum or shasum is required to verify the download"
  fi
}

path_hint() {
  local dir="$1" rc
  echo
  echo "$dir is not on your PATH. To add it:"
  case "$(basename "${SHELL:-}")" in
    fish)
      echo "  fish_add_path $dir"
      return
      ;;
    zsh) rc="~/.zshrc" ;;
    bash) if [ "$(uname -s)" = "Darwin" ]; then rc="~/.bash_profile"; else rc="~/.bashrc"; fi ;;
    *) rc="your shell's startup file" ;;
  esac
  echo "  echo 'export PATH=\"$dir:\$PATH\"' >> $rc"
  echo "then open a new terminal."
}

die() {
  echo "session install: $*" >&2
  exit 1
}

main "$@"
