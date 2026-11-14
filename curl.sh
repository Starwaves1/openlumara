#!/bin/sh
# -- AI GENERATED CODE (Qwen3.8 Flash Next) :: (2026-10-26) harness:openlumara
# OpenLumara quick installer - run it with curl:
#   curl -fsSL https://raw.githubusercontent.com/Rose22/openlumara/main/curl.sh | sh
#
# Optional environment overrides:
#   REPO_URL=...     fork or alternate repo to clone
#   BRANCH=...       branch to install (default: main)
#   INSTALL_DIR=...  where to clone (default: ~/openlumara)
#   MIN_PY=...       minimum python version required (default: 3.12)

set -eu

REPO_URL="${REPO_URL:-https://github.com/Rose22/openlumara}"
BRANCH="${BRANCH:-main}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/openlumara}"
MIN_PY="${MIN_PY:-3.12}"

# soft pink branding, disabled when output is not a terminal
if [ -t 1 ]; then
    C_PINK=$(printf '\033[35m'); C_YEL=$(printf '\033[33m')
    C_RED=$(printf '\033[31m');  C_GRN=$(printf '\033[32m'); C_OFF=$(printf '\033[0m')
else
    C_PINK=""; C_YEL=""; C_RED=""; C_GRN=""; C_OFF=""
fi

say()  { printf "%s[openlumara]%s %s\n" "$C_PINK" "$C_OFF" "$1"; }
warn() { printf "%s[openlumara]%s %s\n" "$C_YEL" "$C_OFF" "$1" >&2; }
die()  { printf "%s[openlumara]%s %s\n" "$C_RED" "$C_OFF" "$1" >&2; exit 1; }

say "welcome! installing OpenLumara..."

# privilege helper for package installs
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
elif command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
else
    SUDO=""
    warn "not running as root and sudo not found - package installs may fail"
fi

# fetch a URL to stdout using curl or wget
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "$1"
    else
        die "neither curl nor wget found - please install one of them first"
    fi
}

# true if $1 is a python binary new enough for MIN_PY
py_ok() {
    command -v "$1" >/dev/null 2>&1 || return 1
    "$1" -c "import sys; sys.exit(0 if sys.version_info >= tuple(int(x) for x in '${MIN_PY}'.split('.')) else 1)" 2>/dev/null
}

# try to install git + python via whatever package manager this distro has
pkg_install() {
    if command -v apt-get >/dev/null 2>&1; then
        $SUDO apt-get update -qq && $SUDO apt-get install -y git python3 python3-venv python3-pip
    elif command -v dnf >/dev/null 2>&1; then
        $SUDO dnf install -y git python3.12 python3-pip || $SUDO dnf install -y git python3 python3-pip
    elif command -v pacman >/dev/null 2>&1; then
        $SUDO pacman -Sy --noconfirm --needed git python
    elif command -v zypper >/dev/null 2>&1; then
        $SUDO zypper install -y git python312 || $SUDO zypper install -y git python3
    elif command -v apk >/dev/null 2>&1; then
        $SUDO apk add git python3 py3-pip
    elif command -v brew >/dev/null 2>&1; then
        brew install git python@3.12
    else
        return 1
    fi
}

# --- git ------------------------------------------------------------------
if ! command -v git >/dev/null 2>&1; then
    say "git not found - attempting to install it..."
    pkg_install || die "could not install git automatically. Install git, then re-run this script."
    command -v git >/dev/null 2>&1 || die "git still not available after install - please install it manually."
fi

# --- python ---------------------------------------------------------------
PYTHON_BIN=""
for cand in python3.13 python3.12 python3 python; do
    if py_ok "$cand"; then PYTHON_BIN="$cand"; break; fi
done

if [ -z "$PYTHON_BIN" ]; then
    if command -v uv >/dev/null 2>&1; then
        # uv ships a standalone CPython, no package manager needed
        say "python ${MIN_PY}+ not found - using uv to install it..."
        uv python install "$MIN_PY"
        UV_PY="$(uv python find "$MIN_PY")"
        py_ok "$UV_PY" || die "uv installed python but it does not work - please install Python ${MIN_PY} manually."
        PYTHON_BIN="$UV_PY"
    else
        say "python ${MIN_PY}+ not found - attempting to install it..."
        pkg_install || warn "package manager install failed or too old - trying uv fallback..."

        # re-check after distro install
        for cand in python3.13 python3.12 python3 python; do
            if py_ok "$cand"; then PYTHON_BIN="$cand"; break; fi
        done

        if [ -z "$PYTHON_BIN" ]; then
            # universal fallback: uv installs a standalone CPython anywhere
            say "falling back to uv to install python ${MIN_PY}..."
            fetch https://astral.sh/uv/install.sh | sh
            PATH="$HOME/.local/bin:$PATH"
            command -v uv >/dev/null 2>&1 || die "uv install failed - please install Python ${MIN_PY} manually, then re-run this script."
            uv python install "$MIN_PY"
            UV_PY="$(uv python find "$MIN_PY")"
            py_ok "$UV_PY" || die "uv installed python but it does not work - please install Python ${MIN_PY} manually."
            PYTHON_BIN="$UV_PY"
        fi
    fi
fi

say "using $PYTHON_BIN ($($PYTHON_BIN -c 'import platform; print(platform.python_version())'))"
export PYTHON_BIN

# --- source code ----------------------------------------------------------
if [ -d "$INSTALL_DIR/.git" ]; then
    say "found existing install at $INSTALL_DIR - updating..."
    if ! git -C "$INSTALL_DIR" pull --ff-only < /dev/null; then
        warn "could not fast-forward (local changes or diverged branch) - continuing with existing checkout"
    fi
elif [ -e "$INSTALL_DIR" ]; then
    die "$INSTALL_DIR exists but is not a git checkout. Remove it or choose another location: INSTALL_DIR=~/other sh <(curl ...)"
else
    say "cloning $REPO_URL (branch: $BRANCH) into $INSTALL_DIR..."
    git clone --depth 1 --branch "$BRANCH" "$REPO_URL" "$INSTALL_DIR" < /dev/null \
        || die "clone failed - check your internet connection, or that the repo/branch exists"
fi

cd "$INSTALL_DIR"
say "installing dependencies (this can take a minute)..."
sh ./install.sh < /dev/null || die "dependency install failed - see output above. retry with: sh $INSTALL_DIR/install.sh"

echo
say "${C_GRN}OpenLumara is installed!${C_OFF}"
say "start it with:  openlumara"
say "(the command was set up automatically; if your shell can't find it yet,"
say " reopen your shell or run:  export PATH=\"\$HOME/.local/bin:\$PATH\")"
