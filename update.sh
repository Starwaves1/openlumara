#!/bin/sh
# the repository root is the directory this script lives in
cd "$(dirname "$0")" || exit 1

# shared helpers for venv management
. tools/venv.sh

echo "checking for updates..."
if git fetch origin 2>/dev/null; then
    if git rev-parse --abbrev-ref @{u} >/dev/null 2>&1; then
        LOCAL=$(git rev-parse HEAD)
        REMOTE=$(git rev-parse @{u})

        if [ "$LOCAL" != "$REMOTE" ]; then
            echo "updates available! pulling changes..."
            git pull
        else
            echo "already up to date."
        fi
    else
        echo "no upstream configured, skipping update check."
    fi
else
    echo "warning: git fetch failed. skipping update check."
fi

echo "ensuring dependencies are up to date..."
if have_uv; then
    uv_sync || exit 1
else
    ensure_venv update || exit 1
fi

echo
echo "done!"
