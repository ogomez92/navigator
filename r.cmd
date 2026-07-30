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

rem Event sounds live in <exe_dir>\navigator_sounds. sync (not copy) so a file
rem removed from the repo folder also disappears from the install.
for %%I in ("%dst%") do set "sounds_dst=%%~dpInavigator_sounds"
rclone sync navigator_sounds "%sounds_dst%"
if errorlevel 1 exit /b %errorlevel%

echo synced navigator_sounds -^> %sounds_dst%
