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
"%BRIDGE%"
