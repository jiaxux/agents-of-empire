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
ok "$(tmux -V), node $(node --version), $(cargo --version | cut -d' ' -f1-2)"

step "Building aoe (the first build takes about 8 minutes, later ones are faster)"
cd "$REPO_DIR"
cargo build --release --locked --features serve

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
