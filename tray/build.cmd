@echo off
rem Build the tray manager with the .NET Framework compiler Windows ships,
rem then assemble the portable package: release\CCProxy\ (exe + embedded node + dist).
rem Usage: build.cmd

setlocal
set CSC=%WINDIR%\Microsoft.NET\Framework64\v4.0.30319\csc.exe
if not exist "%CSC%" set CSC=%WINDIR%\Microsoft.NET\Framework\v4.0.30319\csc.exe
if not exist "%CSC%" (
  echo .NET Framework 4.x compiler not found.
  exit /b 1
)

set OUTDIR=%~dp0
"%CSC%" /nologo /target:winexe /out:"%OUTDIR%CCProxyTray.exe" ^
  /r:System.dll /r:System.Core.dll /r:System.Drawing.dll /r:System.Windows.Forms.dll ^
  "%OUTDIR%Tray.cs"

if errorlevel 1 (
  echo Build failed.
  exit /b 1
)
echo Built %OUTDIR%CCProxyTray.exe

rem ---- assemble portable package: release\CCProxy ----
set ROOT=%~dp0..
set PKG=%ROOT%\release\CCProxy
if exist "%PKG%" rmdir /s /q "%PKG%"
mkdir "%PKG%\node" 2>nul

copy /y "%OUTDIR%CCProxyTray.exe" "%PKG%" >nul || goto :pack_fail
xcopy /e /i /y "%ROOT%\dist" "%PKG%\dist" >nul || goto :pack_fail
if not exist "%PKG%\dist\models.json" copy /y "%ROOT%\src\models.json" "%PKG%\dist\models.json" >nul
copy /y "%ROOT%\package.json" "%PKG%" >nul || goto :pack_fail

set "NODE_SRC="
for /f "delims=" %%N in ('where node 2^>nul') do (
  if not defined NODE_SRC set "NODE_SRC=%%N"
)
if defined NODE_SRC (
  copy /y "%NODE_SRC%" "%PKG%\node\node.exe" >nul || goto :pack_fail
) else (
  echo WARN: node.exe not found on PATH - package has no embedded node.
)

echo Packaged %PKG%
exit /b 0

:pack_fail
echo Packaging failed.
exit /b 1
