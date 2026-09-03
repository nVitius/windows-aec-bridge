@echo off
setlocal
set "BRIDGE=%~dp0aec-bridge-cli.exe"
if not exist "%BRIDGE%" set "BRIDGE=%~dp0target\release\aec-bridge-cli.exe"
if not exist "%BRIDGE%" (
  echo Could not find aec-bridge-cli.exe.
  echo Build it with: cargo build --release --bin aec-bridge-cli
  pause
  exit /b 1
)
echo Active endpoints and formats:
"%BRIDGE%" list
echo.
echo Open AEC Bridge to select and validate the microphone, reference, and virtual handoff.
pause
