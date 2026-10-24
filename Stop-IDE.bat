@echo off
rem Thin shim - all logic lives in open-harness.exe (Rust, launcher/ crate)
cd /d "%~dp0"
open-harness.exe stop
pause
