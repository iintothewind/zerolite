@echo off
REM Windows entrypoint for reclaim-cargo-disk.sh - prefers Git Bash when present.
setlocal EnableExtensions

cd /d "%~dp0..\.."
if errorlevel 1 exit /b 1

set "GIT_BASH=%ProgramFiles%\Git\bin\bash.exe"
if exist "%GIT_BASH%" goto run_bash

REM Avoid "Program Files (x86)" inside an IF - parentheses break cmd parsing.
set "GIT_BASH=%SystemDrive%\Progra~2\Git\bin\bash.exe"
if exist "%GIT_BASH%" goto run_bash

echo Git Bash not found; using cmd fallback.
if exist "target\" rmdir /s /q "target"
if exist "target-docker\" rmdir /s /q "target-docker"
if exist ".cache\npm\" rmdir /s /q ".cache\npm"
if exist "%TEMP%\zc-cargo-home\" rmdir /s /q "%TEMP%\zc-cargo-home"
if exist "%TEMP%\zeroclaw\" rmdir /s /q "%TEMP%\zeroclaw"
del /q "%TEMP%\zc-*" 2>nul
for /d %%D in ("%TEMP%\zc-*") do if exist "%%~fD\" rmdir /s /q "%%~fD"
if defined CARGO_TARGET_DIR if exist "%CARGO_TARGET_DIR%\" rmdir /s /q "%CARGO_TARGET_DIR%"
echo disk reclaim complete
exit /b 0

:run_bash
"%GIT_BASH%" "%~dp0reclaim-cargo-disk.sh"
exit /b %ERRORLEVEL%
