@echo off
setlocal

cd /d "%~dp0"

cargo build --release
if errorlevel 1 exit /b %errorlevel%

set "src=target\release\navigator.exe"
if not defined NAVIGATOR_INSTALL set "NAVIGATOR_INSTALL=%USERPROFILE%\stuff\bin\x.exe"
set "dst=%NAVIGATOR_INSTALL%"

for %%I in ("%dst%") do if not exist "%%~dpI" mkdir "%%~dpI"
copy /Y "%src%" "%dst%" >nul
if errorlevel 1 exit /b %errorlevel%

echo copied %src% -^> %dst%
