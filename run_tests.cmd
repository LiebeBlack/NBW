@echo off
set "CARGO=C:\Users\runneradmin\.cargo\bin\cargo.exe"
if not exist "%CARGO%" exit /b 1
"%CARGO%" test --release --all-features -- --nocapture
