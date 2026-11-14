# shared helpers for install.sh, run.sh and update.sh
#
# The caller changes to the repository root before sourcing this file, so
# every path below is relative to it.
#
# The python environment is in .venv/ for both uv and the plain pip fallback

# minimum python version Openlumara runs on
MIN_PY="3.12"

# is $1 a python version new enough to run OpenLumara?
py_ok() {
    command -v "$1" >/dev/null 2>&1 || return 1
    "$1" -c "import sys; sys.exit(0 if sys.version_info >= tuple(int(x) for x in '${MIN_PY}'.split('.')) else 1)" 2>/dev/null
}

# does uv manage the environment?
have_uv() {
    command -v uv >/dev/null 2>&1
}

# sync the environment with uv
uv_sync() {
    # honour the interpreter the caller picked, so uv and the pip fallback
    # end up using the same one
    if [ -n "${PYTHON_BIN:-}" ]; then
        UV_PYTHON="$PYTHON_BIN"
        export UV_PYTHON
    fi

    # --inexact keeps module dependencies the app installed at runtime
    uv sync --frozen --inexact
}

# pick an interpreter new enough to run Openlumara, into $PYTHON
find_python() {
    if [ -n "${PYTHON_BIN:-}" ] && py_ok "$PYTHON_BIN"; then
        PYTHON="$PYTHON_BIN"
        return 0
    fi

    for cand in python3.13 python3.12 python3 python; do
        if py_ok "$cand"; then
            PYTHON="$cand"
            return 0
        fi
    done
    return 1
}

require_python() {
    if find_python; then
        return 0
    fi

    echo "error: no python ${MIN_PY}+ found (checked python3.13, python3.12, python3, python)"
    echo "       hint: use the quick installer, it installs python for you:"
    echo "             curl -fsSL https://raw.githubusercontent.com/Rose22/openlumara/main/curl.sh | sh"
    return 1
}

# create the environment from scratch
create_venv() {
    echo "setting up virtual environment with $PYTHON ($($PYTHON -c 'import platform; print(platform.python_version())'))..."
    "$PYTHON" -m venv .venv || return 1
    .venv/bin/pip install --upgrade pip || return 1
}

# install the pinned requirements into the environment
install_deps() {
    .venv/bin/python tools/dump_requirements.py --output .venv/requirements.txt || return 1
    .venv/bin/pip install -r .venv/requirements.txt
}

# make sure the environment is ready to run the app
#
# the requirements are installed when the environment is created; pass
# "update" to install them even when the environment already existed
ensure_venv() {
    if [ ! -x .venv/bin/python ]; then
        require_python || return 1
        create_venv || return 1
    elif [ "${1:-}" != "update" ]; then
        return 0
    fi

    install_deps
}
