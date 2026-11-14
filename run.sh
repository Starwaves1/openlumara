#!/bin/sh
# install.sh symlinks this script into ~/.local/bin as `openlumara`, so follow
# the link back to the repository before touching any relative path
target="$0"
while [ -L "$target" ]; do
    link=$(readlink "$target")
    case "$link" in
        /*) target="$link" ;;
        *)  target="$(dirname "$target")/$link" ;;
    esac
done
cd "$(dirname "$target")" || exit 1

# shared helpers for venv management
. tools/venv.sh

# uv manages the environment when available, plain venv + pip otherwise
if have_uv; then
    # build the environment on first run only: syncing on every start puts
    # back the dependencies the app uninstalls for disabled modules
    [ -x .venv/bin/python ] || uv_sync || exit 1
    exec uv run --frozen --no-sync main.py "$@"
fi

ensure_venv || exit 1
exec .venv/bin/python main.py "$@"
