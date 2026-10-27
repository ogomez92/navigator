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

rem The exe loads prism.dll from its own folder, and that folder is shared
rem with other prism users. Ship the one this build was linked against: a
rem different PrismConfig layout corrupts memory rather than failing.
rem Another prism user (noty) may be running and holding the DLL open, in
rem which case the copy fails — fine when what is there is already identical.
for %%I in ("%dst%") do set "prism_dst=%%~dpIprism.dll"
copy /Y "target\release\prism.dll" "%prism_dst%" >nul 2>&1
if errorlevel 1 (
    fc /b "target\release\prism.dll" "%prism_dst%" >nul 2>&1
    if errorlevel 1 (
        echo prism.dll is in use and differs from the one this build needs; close the program using it and rerun
        exit /b 1
    )
    echo prism.dll in use but already current
) else (
    echo copied prism.dll -^> %prism_dst%
)

rem Event sounds live in <exe_dir>\navigator_sounds. sync (not copy) so a file
rem removed from the repo folder also disappears from the install.
for %%I in ("%dst%") do set "sounds_dst=%%~dpInavigator_sounds"
rclone sync navigator_sounds "%sounds_dst%"
if errorlevel 1 exit /b %errorlevel%

echo synced navigator_sounds -^> %sounds_dst%
