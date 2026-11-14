@echo off
rem Shared helpers for run.bat and update.bat.
rem
rem The python environment is in .venv\ for both uv and the plain pip fallback
rem
rem usage: call tools\venv.bat :label
rem
rem   :have_uv          - succeeds when uv is on the PATH
rem   :uv_sync          - syncs the environment with uv
rem   :note_legacy_venv - mentions a leftover venv\
rem   :create_venv      - creates .venv\ when it is missing
rem   :install_deps     - installs the pinned requirements into .venv\
rem   :ensure_venv      - makes sure .venv\ exists and holds the requirements
rem   :refresh_venv     - like :ensure_venv, but always reinstalls them

if "%~1"=="" goto :eof
goto %~1

:have_uv
where uv >nul 2>nul
if errorlevel 1 exit /b 1
exit /b 0

:uv_sync
rem --inexact keeps module dependencies the app installed at runtime
rem core dependencies only: channels and modules install their own on start
uv sync --frozen --inexact
exit /b %errorlevel%

:note_legacy_venv
if not exist "venv" exit /b 0
if exist ".venv" exit /b 0
echo found a legacy venv\! building a fresh .venv\ next to it
echo the old venv folder has been preserved. delete it whenever you\'re ready.
exit /b 0

:create_venv
if exist ".venv\Scripts\python.exe" exit /b 0
where python >nul 2>nul
if errorlevel 1 (
    echo error: no python binary found. please make sure python is installed and in your PATH.
    pause
    exit /b 1
)
echo setting up virtual environment...
python -m venv .venv
if errorlevel 1 exit /b 1
.venv\Scripts\python -m pip install -q --upgrade pip
if errorlevel 1 exit /b 1
exit /b 0

:install_deps
.venv\Scripts\python tools\dump_requirements.py --output .venv\requirements.txt
if errorlevel 1 exit /b 1
.venv\Scripts\python -m pip install -r .venv\requirements.txt
if errorlevel 1 exit /b 1
exit /b 0

:ensure_venv
if exist ".venv\Scripts\python.exe" exit /b 0
call "%~dp0venv.bat" :create_venv
if errorlevel 1 exit /b 1
call "%~dp0venv.bat" :install_deps
if errorlevel 1 exit /b 1
exit /b 0

:refresh_venv
call "%~dp0venv.bat" :create_venv
if errorlevel 1 exit /b 1
call "%~dp0venv.bat" :install_deps
if errorlevel 1 exit /b 1
exit /b 0
