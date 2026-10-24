@echo off
rem Thin shim - all logic lives in open-harness.exe (Rust, launcher/ crate).
rem "app" starts the engine and opens the Open Harness window.
cd /d "%~dp0"
open-harness.exe app
pause
