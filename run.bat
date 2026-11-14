@echo off
setlocal enabledelayedexpansion

:: Switch to the directory where this .bat file is located
cd /d "%~dp0"

:: uv manages the environment when available, plain venv + pip otherwise
call tools\venv.bat :have_uv
if not errorlevel 1 goto :uv

echo uv not found, using venv + pip...
call tools\venv.bat :ensure_venv
if errorlevel 1 exit /b 1

echo starting openlumara...
.venv\Scripts\python main.py %*
goto :check

:uv
call tools\venv.bat :uv_sync
if errorlevel 1 exit /b 1
echo starting openlumara...
uv run --frozen --no-sync main.py %*

:check
if !errorlevel! neq 0 (
    echo.
    echo an error occurred while running the application.
    pause
)
