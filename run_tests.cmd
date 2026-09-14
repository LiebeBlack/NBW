@echo off
where cargo >nul 2>&1
if %errorlevel%==0 (
    set "CARGO=cargo"
) else if exist "C:\Users\runneradmin\.cargo\bin\cargo.exe" (
    set "CARGO=C:\Users\runneradmin\.cargo\bin\cargo.exe"
) else (
    echo cargo not found; install Rust or add cargo to PATH
    exit /b 1
)
"%CARGO%" test --release --all-features -- --nocapture
