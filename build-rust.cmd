@echo off
rem Build the Rust rewrite and publish it to the hot-swap folder.
rem
rem This is the Rust counterpart of build.cmd: same output layout, so the tray
rem and the handover protocol are unchanged, but the package is two binaries
rem instead of a bundled Node runtime.
rem
rem Outputs (version-stamped, so a hot update is unambiguous):
rem   release\CCProxyRust\                             local staging package
rem   <Desktop>\CCProxy-Release\CCProxy-<ver>-rust\    this build, ready to drop in
rem   <Desktop>\CCProxy-Release\CCProxy-current\       ONLY with --promote
rem
rem The Node package publishes to CCProxy-<ver>\ and keeps older folders for
rem rollback. This script deliberately does NOT move CCProxy-current by default:
rem building a candidate and switching production are different acts, and the
rem second one should be a decision rather than a side effect. Pass --promote to
rem switch after a verified build.
rem
rem Usage: build-rust.cmd [version] [--promote]
rem        version defaults to the Cargo workspace version.

setlocal enabledelayedexpansion
set ROOT=%~dp0

rem ---- arguments: an optional version and an optional --promote ----
set VER=
set PROMOTE=
for %%A in (%*) do (
  if /i "%%A"=="--promote" (set PROMOTE=1) else (set VER=%%A)
)
if "%VER%"=="" (
  rem Cargo.toml is TOML, so a JSON parser cannot read it; the version is
  rem stderr is suppressed, and stdout comes back empty. The version is read
  rem off the `version = "x.y.z"` line instead, and validated before use -
  rem a junk value here names folders, and a stray `CCProxy-v<junk>` on the
  rem Desktop is how an unparseable version announces itself.
  set "LINE="
  for /f "tokens=3" %%V in ('findstr /b /c:"version = " "!ROOT!\Cargo.toml" 2^>nul') do (
    if not defined LINE set "LINE=%%V"
  )
  set "LINE=!LINE:"=!"
  set "VER=!LINE!"
  rem Guard: the version names the published folder, so a junk value must stop
  rem the build rather than produce `CCProxy-v<junk>` on the Desktop. A real
  rem version starts with x.y.z; a suffix like `-rc1` is allowed after it.
  echo !VER!| findstr /r /c:"^[0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*" >nul || (
    echo ERROR: could not read a version from Cargo.toml ^(got "!VER!"^).
    echo Pass one explicitly: build-rust.cmd 0.5.0
    exit /b 1
  )
)
if "%VER%"=="" (
  echo Could not determine version.
  exit /b 1
)
echo Building cc-proxy (Rust) v%VER%

rem ---- build both binaries in release mode ----
rem The profile (opt-level=s, lto, codegen-units=1, panic=abort, strip) lives in
rem Cargo.toml; --locked keeps the build reproducible against Cargo.lock.
pushd "!ROOT!"
cargo build --release --locked --bin ccproxy --bin CCProxyTray
if errorlevel 1 (
  popd
  echo Build failed.
  exit /b 1
)
popd

set PROXY=%ROOT%\target\release\ccproxy.exe
set TRAY=%ROOT%\target\release\CCProxyTray.exe
if not exist "%PROXY%" (
  echo ERROR: %PROXY% missing after the build.
  exit /b 1
)
if not exist "%TRAY%" (
  echo ERROR: %TRAY% missing after the build.
  exit /b 1
)

rem ---- staging package ----
rem The tray is the entry point and stays in the root; the proxy it launches
rem lives in service\. Both binaries used to sit side by side, and the first
rem person to open the package double-clicked the proxy — a console program
rem with no UI — and read the result as a broken release. The layout now says
rem which one to run.
set PKG=%ROOT%\release\CCProxyRust
if exist "%PKG%" rmdir /s /q "%PKG%"
mkdir "%PKG%" 2>nul
mkdir "%PKG%\service" 2>nul

copy /y "%PROXY%" "%PKG%\service" >nul || goto :pack_fail
copy /y "%TRAY%" "%PKG%" >nul || goto :pack_fail
rem The outbound-proxy policy lives in this file so a person can read and edit
rem it; without a shipped copy, setting it would mean an environment variable
rem nobody can see. Written rather than copied from the repo root, so a local
rem edit never leaks into a release.
> "%PKG%\service\ccproxy.json" echo {
>> "%PKG%\service\ccproxy.json" echo   "proxy": "default",
>> "%PKG%\service\ccproxy.json" echo   "noProxy": "localhost,127.0.0.1"
>> "%PKG%\service\ccproxy.json" echo }
rem The tray and the proxy both read the version from this file (tray tooltip,
rem /health is compiled in). Written rather than copied so the package carries
rem the workspace version even though it ships no package.json of its own.
> "%PKG%\package.json" echo {"version":"%VER%"}
rem Never ship runtime state produced by local testing.
if exist "%PKG%\selfcheck.log" del /q "%PKG%\selfcheck.log"
if exist "%PKG%\logs" rmdir /s /q "%PKG%\logs"

for %%F in ("%PKG%\service\ccproxy.exe") do echo service\ccproxy.exe: %%~zF bytes
for %%F in ("%PKG%\CCProxyTray.exe") do echo CCProxyTray.exe: %%~zF bytes

rem ---- size gate: the entire point of the rewrite ----
rem 10 MB is the spec's ceiling; the Node package it replaces was ~89 MB. The
rem check exists because a future dependency could quietly pull in half of
rem Windows and nobody would notice until the artifact was published.
set /a MAX_BYTES=10485760
for %%F in ("%PKG%\service\ccproxy.exe") do set /a PROXY_BYTES=%%~zF
if %PROXY_BYTES% GTR %MAX_BYTES% (
  echo ERROR: ccproxy.exe is %PROXY_BYTES% bytes, over the 10 MB budget.
  exit /b 1
)

rem ---- verify the packaged build before publishing it ----
rem The conformance harness is gone (the golden's job ended with the rewrite);
rem an unverified artifact must not reach the Desktop, so run the crate's own
rem test suite as the gate instead.
rem Note: capture the exit code BEFORE popd — an internal command that
succeeded resets errorlevel to 0, which would let a failing test suite
publish anyway.
if defined CC_SKIP_VERIFY goto :verify_skipped
echo Verifying packaged build...
pushd "!ROOT!"
cargo test --locked
set "TEST_EC=!ERRORLEVEL!"
popd
if not "!TEST_EC!"=="0" goto :verify_fail
goto :verify_done

:verify_skipped
echo WARN: CC_SKIP_VERIFY is set - packaging an unverified build.

:verify_done

rem ---- publish to the hot-swap folder on the Desktop ----
rem The Desktop may be redirected (this machine: D:\Windows\Desktop), so ask the
rem shell for the real path instead of assuming %USERPROFILE%\Desktop.
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

set TARGET=%SWAP%\CCProxy-v%VER%-rust
if exist "%TARGET%" rmdir /s /q "%TARGET%"
xcopy /e /i /y "%PKG%" "%TARGET%" >nul || goto :pack_fail

if not defined PROMOTE goto :no_promote
echo.
echo Switching CCProxy-current to the Rust build...
echo NOTE: a running tray keeps serving until the new one takes over; the
echo handover is automatic and the previous version stays on disk for rollback.
set CUR=%SWAP%\CCProxy-current
rem Rename the old pointer aside before replacing it, so an interrupted copy
rem leaves a recoverable folder rather than a half-written CCProxy-current.
if exist "%CUR%" (
  if exist "%CUR%-previous" rmdir /s /q "%CUR%-previous"
  move "%CUR%" "%CUR%-previous" >nul || goto :pack_fail
)
xcopy /e /i /y "%PKG%" "%CUR%" >nul || goto :pack_fail

echo.
echo Hot-swap artifacts (CCProxy-current now points at this build):
echo   %TARGET%
echo   %CUR%
echo.
echo Existing versions kept for rollback:
dir /b /ad "%SWAP%\CCProxy-*" 2>nul
exit /b 0

:no_promote
echo.
echo Published this build, but did NOT switch CCProxy-current:
echo   %TARGET%
echo.
echo The running instance is untouched. To make this the active build:
echo   - run this script again with --promote, or
echo   - swap the folder names on the Desktop yourself.
exit /b 0

:pack_fail
echo Packaging failed.
exit /b 1

:verify_fail
echo.
echo ERROR: the packaged build failed verification - NOT publishing it.
echo The previous versions on the Desktop are untouched, so the running
echo instance is unaffected. Fix the failure above and re-run.
echo (Set CC_SKIP_VERIFY=1 to bypass - only when you know why it fails.)
exit /b 1
