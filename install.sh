#!/bin/sh
#   usage: sh install.sh [--reinstall]
#   --reinstall   discard the existing environment and build a fresh one
# env: PYTHON_BIN=<path>  use this interpreter instead of auto-detecting

# -- AI GENERATED CODE (Qwen3.8 Flash Next) :: (2026-10-26) harness:openlumara
set -eu

# the repository root is the directory this script is in
cd "$(dirname "$0")" || exit 1

# shared helpers for venv management
. tools/venv.sh

# exposes an `openlumara` command by symlinking run.sh into ~/.local/bin
# (done before the venv early-exit so re-running install.sh also repairs it)
setup_command() {
    BIN_DIR="$HOME/.local/bin"
    LINK="$BIN_DIR/openlumara"
    TARGET="$(pwd)/run.sh"
    mkdir -p "$BIN_DIR"
    if [ -L "$LINK" ]; then
        rm -f "$LINK"
        ln -s "$TARGET" "$LINK"
    elif [ -e "$LINK" ]; then
        echo "note: $LINK already exists and is not a symlink - leaving it alone"
    else
        ln -s "$TARGET" "$LINK"
    fi

    # make sure ~/.local/bin is on PATH for future shells (macOS notably lacks it)
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *)
            for rc in "$HOME/.profile" "$HOME/.zshrc"; do
                if [ "$rc" = "$HOME/.zshrc" ] && [ ! -f "$rc" ] && [ "$(uname)" != "Darwin" ]; then
                    continue
                fi
                grep -q '.local/bin' "$rc" 2>/dev/null || {
                    printf '\n# added by OpenLumara installer\nexport PATH="$HOME/.local/bin:$PATH"\n' >> "$rc"
                }
            done
            echo "added ~/.local/bin to PATH - reopen your shell, or run:"
            echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
            ;;
    esac
}
setup_command

if [ "${1:-}" = "--reinstall" ]; then
    echo "removing existing environment..."
    rm -rf .venv
fi

# uv manages the environment when available, plain venv + pip otherwise
if have_uv; then
    echo "setting up the environment with uv..."
    uv_sync || exit 1
    echo "done! start OpenLumara with: openlumara   (or ./run.sh)"
    exit 0
fi

if [ -x .venv/bin/python ]; then
    echo ".venv already set up - nothing to do (use --reinstall to rebuild)"
    echo "start OpenLumara with: openlumara   (or ./run.sh)"
    exit 0
fi

ensure_venv || exit 1
echo "done! start OpenLumara with: openlumara   (or ./run.sh)"
