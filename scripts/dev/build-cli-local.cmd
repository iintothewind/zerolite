@echo off
REM Windows entrypoint for build-cli-local.sh - always uses Git Bash, never WSL.
REM Usage: scripts\dev\build-cli-local.cmd [--skip-web] [--plugins-wasm] [other args...]
setlocal EnableExtensions

set "GIT_BASH=%ProgramFiles%\Git\bin\bash.exe"
if not exist "%GIT_BASH%" set "GIT_BASH=%ProgramFiles(x86)%\Git\bin\bash.exe"
if not exist "%GIT_BASH%" (
  echo error: Git Bash not found. Install Git for Windows, then retry.
  exit /b 1
)

REM scripts\dev -> repo root
cd /d "%~dp0..\.." || exit /b 1

REM Drop sandbox CARGO_TARGET_DIR so collect reads repo target\ after Docker.
set "CARGO_TARGET_DIR="

"%GIT_BASH%" "%~dp0build-cli-local.sh" %*
exit /b %ERRORLEVEL%
