@echo off
rem vmmc Tools for Windows: runs setup.ps1 from this disc (it asks for administrator rights).
start "" powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "%~dp0setup.ps1"
