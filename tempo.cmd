@echo off
setlocal
python "%~dp0tempo.py" %*
exit /b %errorlevel%
