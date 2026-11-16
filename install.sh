#!/usr/bin/env bash
# Build Agent of Empires (aoe) from this checkout and install it.
#
#   ./install.sh                         build and install to ~/.local/bin
#   INSTALL_DIR=~/bin ./install.sh       install somewhere else
#
# Run it again after `git pull` to update. On macOS, missing tools (tmux,
# Node.js, Rust) are installed with Homebrew.
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
OS="$(uname -s)"

step() { printf '\n\033[1;34m==>\033[0m \033[1m%s\033[0m\n' "$*"; }
ok()   { printf '\033[1;32m  ok\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

case "${1:-}" in
  -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  "") ;;
  *) die "unknown option: $1 (try ./install.sh --help)" ;;
esac

# Mirrors get_app_dir_path() in src/session/mod.rs for release builds.
app_dir() {
  local xdg="${XDG_CONFIG_HOME:-$HOME/.config}/agent-of-empires"
  if [ "$OS" != Darwin ]; then
    echo "$xdg"
  elif [ -d "$xdg" ]; then
    echo "$xdg"
  elif [ -d "$HOME/.agent-of-empires" ]; then
    echo "$HOME/.agent-of-empires"
  elif [ -n "${XDG_CONFIG_HOME:-}" ]; then
    echo "$xdg"
  else
    echo "$HOME/.agent-of-empires"
  fi
}

step "Checking tools"
if [ "$OS" = Darwin ] && ! have brew; then
  # A fresh Homebrew install may not be on this shell's PATH yet.
  for b in /opt/homebrew/bin/brew /usr/local/bin/brew; do
    if [ -x "$b" ]; then eval "$("$b" shellenv)"; break; fi
  done
fi

missing=""
have tmux  || missing="$missing tmux"
have npm   || missing="$missing node"
have cargo || missing="$missing rust"

if [ -n "$missing" ]; then
  if [ "$OS" = Darwin ]; then
    have brew || die "Homebrew is needed to install:$missing. Get it from https://brew.sh, then run ./install.sh again."
    echo "Installing$missing with Homebrew..."
    # shellcheck disable=SC2086 # word splitting is intended
    brew install $missing
  else
    die "Missing:$missing. Install them with your package manager (Rust from https://rustup.rs), then run ./install.sh again."
  fi
fi
# `have npm` only proves the file exists. A Homebrew node whose shared
# libraries were upgraded underneath it (llhttp, icu4c) is present but
# can't start, and `npm ci` would otherwise fail minutes into the Rust build.
node_version="$(node --version 2>&1)" \
  || die "node is installed but won't run: ${node_version}. Reinstall it (brew upgrade node, or your package manager), then run ./install.sh again."
ok "$(tmux -V), node $node_version, $(cargo --version | cut -d' ' -f1-2)"

# aws-lc-sys (the TLS library behind reqwest and rustls) refuses GCC 9.x and
# 10.1: they miscompile memcmp at -O3 (https://gcc.gnu.org/bugzilla/show_bug.cgi?id=95189).
# Ubuntu 20.04 still ships GCC 9.4 as `cc`. Run the same probe aws-lc-sys runs
# and, if it fails, build with a newer compiler that is already installed.
memcmp_ok() {
  local dir rc=1
  dir="$(mktemp -d)"
  printf '%s\n' '#include <stdio.h>' '#include <string.h>' \
    'int main(void) { char a[] = "\0abc"; int r = memcmp(a, "\0\0\0\0", 4); printf("%d\n", r); return r == 0; }' \
    > "$dir/probe.c"
  if "$1" -O3 -o "$dir/probe" "$dir/probe.c" 2>/dev/null && "$dir/probe" >/dev/null; then rc=0; fi
  rm -rf "$dir"
  return $rc
}

if [ "$OS" != Darwin ]; then
  have "${CC:-cc}" || die "No C compiler found. Install one (e.g. sudo apt install build-essential), then run ./install.sh again."
  if ! memcmp_ok "${CC:-cc}"; then
    for c in gcc-15 gcc-14 gcc-13 gcc-12 gcc-11 gcc-10 clang; do
      have "$c" && memcmp_ok "$c" || continue
      export CC="$c"
      case "$c" in
        gcc-*) have "g++-${c#gcc-}" && export CXX="g++-${c#gcc-}" ;;
        clang) have clang++ && export CXX=clang++ ;;
      esac
      break
    done
    [ -n "${CC:-}" ] && memcmp_ok "$CC" \
      || die "$(${CC:-cc} --version | head -1) miscompiles memcmp (GCC bug 95189), so the TLS library won't build with it. Install a newer compiler (e.g. sudo apt install gcc-11, or clang) and run ./install.sh again, or pick one explicitly: CC=gcc-11 ./install.sh"
    ok "building with CC=$CC (the default C compiler has GCC bug 95189)"
  fi
fi

# Homebrew on Linux ships its own glibc, and its `ld` and `pkg-config` come
# first on PATH. pkg-config then hands cargo Homebrew's libgit2 and liblzma,
# built for that newer glibc, and linking them into a binary that runs on the
# system glibc fails with `undefined reference to ...@GLIBC_2.34`. For the
# build, point pkg-config at the system .pc files (the PKG_CONFIG* variables,
# unlike PATH, also make cargo redo cached *-sys probes) and shadow just the
# binutils with the system ones; node, npm, cargo, and rustc keep resolving as
# they do now (Homebrew's are fine, they only run).
build_path="$PATH"
if [ "$OS" != Darwin ] && have brew && [ -x /usr/bin/pkg-config ]; then
  export PKG_CONFIG=/usr/bin/pkg-config
  PKG_CONFIG_LIBDIR="$(/usr/bin/pkg-config --variable pc_path pkg-config)"
  export PKG_CONFIG_LIBDIR
  sysbin="$(mktemp -d)"
  trap 'rm -rf "$sysbin"' EXIT
  for t in ld ar ranlib nm objcopy strip; do
    [ -x "/usr/bin/$t" ] && ln -s "/usr/bin/$t" "$sysbin/$t"
  done
  build_path="$sysbin:$PATH"
  ok "linking with the system binutils and pkg-config, not Homebrew's"
fi

step "Building aoe (the first build takes about 8 minutes, later ones are faster)"
cd "$REPO_DIR"
PATH="$build_path" cargo build --release --locked --features serve

step "Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR" 2>/dev/null || true
[ -w "$INSTALL_DIR" ] || die "Can't write to $INSTALL_DIR. Pick another folder, e.g. INSTALL_DIR=~/bin ./install.sh"
# Copy, then rename into place: a running aoe keeps its old file, and macOS
# never sees a half-written binary.
tmp="$INSTALL_DIR/.aoe.tmp.$$"
cp target/release/aoe "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$INSTALL_DIR/aoe"
ok "$("$INSTALL_DIR/aoe" --version) at $INSTALL_DIR/aoe"

# The update banner and `aoe update` download the stock upstream release,
# which would replace this build. Updates come from `git pull` instead.
step "Turning off upstream update checks"
cfg="$(app_dir)/config.toml"
if [ ! -f "$cfg" ]; then
  mkdir -p "$(dirname "$cfg")"
  (umask 077; printf '[updates]\nupdate_check_mode = "off"\n' > "$cfg")
  ok "created $cfg"
elif grep -q '^[[:space:]]*update_check_mode' "$cfg"; then
  ok "kept your existing update_check_mode in $cfg"
elif grep -q '^\[updates\][[:space:]]*$' "$cfg"; then
  updated="$(awk '{ print } /^\[updates\][[:space:]]*$/ { print "update_check_mode = \"off\"" }' "$cfg")"
  printf '%s\n' "$updated" > "$cfg"
  ok "updated $cfg"
else
  printf '\n[updates]\nupdate_check_mode = "off"\n' >> "$cfg"
  ok "updated $cfg"
fi

new_shell=0
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    line="export PATH=\"$INSTALL_DIR:\$PATH\""
    case "${SHELL##*/}" in
      zsh) rc="$HOME/.zshrc" ;;
      bash) if [ "$OS" = Darwin ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
      *) rc="" ;;
    esac
    if [ -n "$rc" ]; then
      grep -qsF "$line" "$rc" || printf '\n# Added by agent-of-empires install.sh\n%s\n' "$line" >> "$rc"
      ok "added $INSTALL_DIR to PATH in $rc"
      new_shell=1
    else
      warn "add $INSTALL_DIR to your PATH: $line"
    fi
    ;;
esac

other="$(command -v aoe 2>/dev/null || true)"
if [ -n "$other" ] && [ "$other" != "$INSTALL_DIR/aoe" ]; then
  warn "another aoe at $other comes first on your PATH. Remove it (brew uninstall aoe, or rm \"$other\") so this build is the one that runs."
fi

if [ -f "$(app_dir)/serve.pid" ]; then
  warn "the web dashboard is still running the old version. Restart it: aoe serve --stop, then start it again."
fi

step "Done"
if [ "$new_shell" = 1 ]; then
  echo "Open a new terminal window first, so it picks up the new PATH."
fi
cat <<'EOF'
  aoe            open the terminal dashboard
  aoe serve      start the web dashboard
  ./install.sh   run again after `git pull` to update
EOF
