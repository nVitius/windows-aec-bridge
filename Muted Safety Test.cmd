@echo off
setlocal
set "BRIDGE=%~dp0aec-bridge.exe"
if not exist "%BRIDGE%" set "BRIDGE=%~dp0target\release\aec-bridge.exe"
if not exist "%BRIDGE%" (
  echo Could not find aec-bridge.exe.
  echo Build it with: cargo build --release --bin aec-bridge
  pause
  exit /b 1
)
echo In AEC Bridge, select the three endpoints, open Advanced, and enable Mute handoff.
echo Start the bridge, verify all three streams report ready, then click Stop.
pause
"%BRIDGE%"
pause
