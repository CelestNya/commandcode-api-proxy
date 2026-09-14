@echo off
rem Build the tray manager with the .NET Framework compiler Windows ships,
rem then assemble a portable package and publish it to the hot-swap folder.
rem
rem Outputs (version-stamped, so a hot update is unambiguous):
rem   release\CCProxy\                                  local staging package
rem   <Desktop>\CCProxy-Release\CCProxy-<ver>\          this build, ready to drop in
rem   <Desktop>\CCProxy-Release\CCProxy-current\        symlink-free copy of the newest
rem The previous version folder is kept, so there is always a known-good
rem artifact to fall back to if a hot update misbehaves.
rem
rem Usage: build.cmd [version]      (default: read TrayVersion from Tray.cs)

setlocal enabledelayedexpansion
set CSC=%WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe
if not exist "%CSC%" set CSC=%WINDIR%\Microsoft.NET\Framework\v4.0.30319\csc.exe
if not exist "%CSC%" (
  echo .NET Framework 4.x compiler not found.
  exit /b 1
)

set OUTDIR=%~dp0
set ROOT=%~dp0..

rem ---- version: argument, else package.json (single source of truth) ----
rem Backslashes must be converted to forward slashes: cmd eats "\U" style
rem sequences inside the nested quotes, which silently corrupts the path and
rem makes node print the literal expression instead of the version.
set VER=%~1
if "%VER%"=="" (
  set "ROOTFS=!ROOT:\=/!"
  for /f "delims=" %%V in ('node -p "require('!ROOTFS!/package.json').version" 2^>nul') do set "VER=%%V"
)
if "%VER%"=="" (
  rem Fallback: extract TrayVersionFallback from source if node is unavailable
  for /f "tokens=2 delims==" %%V in ('findstr /r /c:"TrayVersionFallback =" "%OUTDIR%Tray.cs"') do (
    set "LINE=%%V"
  )
  set "LINE=!LINE: =!"
  set "LINE=!LINE:"=!"
  set "LINE=!LINE:;=!"
  set VER=!LINE!
)
if "%VER%"=="" (
  echo Could not determine version.
  exit /b 1
)

"%CSC%" /nologo /target:winexe /out:"%OUTDIR%CCProxyTray.exe" ^
  /r:System.dll /r:System.Core.dll /r:System.Drawing.dll /r:System.Windows.Forms.dll ^
  "%OUTDIR%Tray.cs"
if errorlevel 1 (
  echo Build failed.
  exit /b 1
)
echo Built CCProxyTray.exe (v%VER%)

rem ---- staging package ----
rem Refuse to package a stale dist: a deleted module still sitting in dist/
rem ships dead files (v0.4.1-p1 went out carrying dist\auth.js + dist\setup\
rem long after those were removed). Rebuild first, then package.
rem Note: no parentheses inside these echo lines — cmd would read them as the
rem end of the enclosing block.
if not exist "%ROOT%\dist\proxy.js" goto :stale_dist_missing
if exist "%ROOT%\dist\auth.js" goto :stale_dist_auth
if exist "%ROOT%\dist\setup" goto :stale_dist_setup

set PKG=%ROOT%\release\CCProxy
if exist "%PKG%" rmdir /s /q "%PKG%"
mkdir "%PKG%\node" 2>nul

copy /y "%OUTDIR%CCProxyTray.exe" "%PKG%" >nul || goto :pack_fail
xcopy /e /i /y "%ROOT%\dist" "%PKG%\dist" >nul || goto :pack_fail
if not exist "%PKG%\dist\models.json" copy /y "%ROOT%\src\models.json" "%PKG%\dist\models.json" >nul
copy /y "%ROOT%\package.json" "%PKG%" >nul || goto :pack_fail
rem Never ship runtime state produced by local testing.
if exist "%PKG%\selfcheck.log" del /q "%PKG%\selfcheck.log"
if exist "%PKG%\logs" rmdir /s /q "%PKG%\logs"

set "NODE_SRC="
for /f "delims=" %%N in ('where node 2^>nul') do (
  if not defined NODE_SRC set "NODE_SRC=%%N"
)
if defined NODE_SRC (
  copy /y "%NODE_SRC%" "%PKG%\node\node.exe" >nul || goto :pack_fail
) else (
  echo WARN: node.exe not found on PATH - package has no embedded node.
)

rem ---- publish to the hot-swap folder on the Desktop ----
rem The Desktop may be redirected (this machine: D:\Windows\Desktop), so ask
rem the shell for the real path instead of assuming %USERPROFILE%\Desktop.
set "DESK="
for /f "tokens=2,*" %%A in ('reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders" /v Desktop 2^>nul ^| findstr /i Desktop') do set "DESK=%%B"
if defined DESK call set "DESK=%DESK%"
if not defined DESK set "DESK=%USERPROFILE%\Desktop"
if not exist "%DESK%" (
  echo WARN: Desktop not found at "%DESK%"; skipping hot-swap publish.
  echo Packaged %PKG%
  exit /b 0
)
set SWAP=%DESK%\CCProxy-Release
if not exist "%SWAP%" mkdir "%SWAP%"

set TARGET=%SWAP%\CCProxy-v%VER%
if exist "%TARGET%" rmdir /s /q "%TARGET%"
xcopy /e /i /y "%PKG%" "%TARGET%" >nul || goto :pack_fail

rem CCProxy-current mirrors the newest build (plain copy: no symlink privileges needed)
set CUR=%SWAP%\CCProxy-current
if exist "%CUR%" rmdir /s /q "%CUR%"
xcopy /e /i /y "%PKG%" "%CUR%" >nul || goto :pack_fail

echo.
echo Hot-swap artifacts:
echo   %TARGET%
echo   %CUR%
echo.
echo Existing versions kept for rollback:
dir /b /ad "%SWAP%\CCProxy-*" 2>nul
exit /b 0

:pack_fail
echo Packaging failed.
exit /b 1

:stale_dist_missing
echo ERROR: dist\proxy.js is missing. Run "pnpm build" first.
exit /b 1

:stale_dist_auth
echo ERROR: dist\auth.js still present - dist is stale. Run a clean "pnpm build".
exit /b 1

:stale_dist_setup
echo ERROR: dist\setup still present - dist is stale. Run a clean "pnpm build".
exit /b 1
