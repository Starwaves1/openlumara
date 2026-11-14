@echo off
setlocal enabledelayedexpansion

:: Switch to the directory where this .bat file is located
cd /d "%~dp0"

:: 1. Smart auto-update
echo checking for updates...
git fetch origin >nul 2>nul
if !errorlevel! equ 0 (
    :: Get local HEAD
    for /f "tokens=*" %%i in ('git rev-parse HEAD') do set LOCAL=%%i

    :: Get remote HEAD (using @{u} for upstream)
    set REMOTE=
    for /f "tokens=*" %%i in ('git rev-parse @{u} 2^>nul') do set REMOTE=%%i

    if "!REMOTE!"=="" (
        echo no upstream configured, skipping update check.
    ) else if "!LOCAL!" NEQ "!REMOTE!" (
        echo updates available! pulling changes...
        git pull
    ) else (
        echo already up to date.
    )
) else (
    echo warning: git fetch failed. skipping update check.
)

:: 2. Ensure dependencies are up to date
echo ensuring dependencies are up to date...
call tools\venv.bat :have_uv
if errorlevel 1 goto :pip

call tools\venv.bat :uv_sync
if errorlevel 1 exit /b 1
goto :done

:: the requirements are reinstalled, so a changed requirements.txt is
:: picked up even when the environment already existed
:pip
echo uv not found, using venv + pip...
call tools\venv.bat :refresh_venv
if errorlevel 1 exit /b 1

:done
echo.
echo done!
