@echo off
rem Sets up Clawdmeter on this PC: the app, Claude Code, Codex and Cursor,
rem their hooks and Start at login. See tools\setup\windows.ps1.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\setup\windows.ps1" %*
pause
