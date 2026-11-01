@echo off
setlocal EnableExtensions EnableDelayedExpansion

rem ===========================================================================
rem service-win.bat - install and manage llm-gateway as a Windows service.
rem
rem llm-gateway.exe has no native Windows service (SCM) integration, so this
rem script wraps it with NSSM (https://nssm.cc): NSSM hosts the service in
rem services.msc, captures stdout/stderr into rotating log files and restarts
rem the gateway if it exits. Run from an Administrator prompt, or just run it
rem and accept the elevation prompt.
rem
rem   service-win.bat install [config-path]
rem   service-win.bat uninstall ^| start ^| stop ^| status
rem ===========================================================================

set "SERVICE_NAME=llm-gateway"
set "DISPLAY_NAME=LLM Gateway"
set "NSSM_VERSION=2.24"
set "NSSM_URL=https://nssm.cc/release/nssm-%NSSM_VERSION%.zip"

for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "EXE=%ROOT%\bin\llm-gateway.exe"
set "NSSM=%ROOT%\bin\nssm.exe"
set "LOG_DIR=%ProgramData%\llm-gateway"

set "ACTION=%~1"

if /i "%ACTION%"=="install" goto admin
if /i "%ACTION%"=="uninstall" goto admin
if /i "%ACTION%"=="start" goto admin
if /i "%ACTION%"=="stop" goto admin
if /i "%ACTION%"=="status" goto status
goto usage

:admin
fltmc >nul 2>&1
if not errorlevel 1 goto run
rem Relaunch this script elevated, preserving the original arguments.
set "ARGSTR="
:collect_args
if "%~1"=="" goto collected_args
set "ARGSTR=!ARGSTR! "%~1""
rem shift /1 (not plain shift): a bare shift would also shift %0, so %~f0
rem below would resolve to the first argument instead of this script.
shift /1
goto collect_args
:collected_args
echo Requesting administrator privileges...
powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -ArgumentList '!ARGSTR!' -Verb RunAs"
exit /b

:run
if /i "%ACTION%"=="install" goto install
if /i "%ACTION%"=="uninstall" goto uninstall
if /i "%ACTION%"=="start" goto svc_start
if /i "%ACTION%"=="stop" goto svc_stop
goto usage

:install
if not exist "%EXE%" (
    echo ERROR: %EXE% not found.
    echo        Build it first:  make build
    echo        or:  go build -trimpath -o bin\llm-gateway.exe .\cmd\llm-gateway
    exit /b 1
)
sc query "%SERVICE_NAME%" >nul 2>&1
if not errorlevel 1 (
    echo ERROR: service "%SERVICE_NAME%" is already installed. Remove it first:
    echo        %~nx0 uninstall
    exit /b 1
)
call :ensure_nssm
if errorlevel 1 exit /b 1

rem Pick a config: explicit argument, then the repo's config.yaml, then the
rem user's default path. With none of those the gateway falls back to the
rem LocalSystem home directory, which is usually not what you want - warn.
set "CONFIG_PATH=%~2"
if not defined CONFIG_PATH if exist "%ROOT%\config.yaml" set "CONFIG_PATH=%ROOT%\config.yaml"
if not defined CONFIG_PATH if exist "%USERPROFILE%\.config\llm-gateway\config.yaml" set "CONFIG_PATH=%USERPROFILE%\.config\llm-gateway\config.yaml"

if not exist "%LOG_DIR%" mkdir "%LOG_DIR%"
if not exist "%LOG_DIR%" (
    echo ERROR: cannot create log directory %LOG_DIR%
    exit /b 1
)

if defined CONFIG_PATH (
    "%NSSM%" install "%SERVICE_NAME%" "%EXE%" --config "%CONFIG_PATH%"
) else (
    echo WARNING: no config file found; the service would read
    echo          %SystemRoot%\System32\config\systemprofile\.config\llm-gateway\config.yaml
    echo          ^(the LocalSystem home directory^). Prefer passing a path:
    echo          %~nx0 install "C:\path\to\config.yaml"
    "%NSSM%" install "%SERVICE_NAME%" "%EXE%"
)
if errorlevel 1 (
    echo ERROR: nssm install failed.
    exit /b 1
)

"%NSSM%" set "%SERVICE_NAME%" DisplayName "%DISPLAY_NAME%"                        || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" Description "OpenAI-compatible virtual-key gateway" || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" Start SERVICE_AUTO_START                            || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppDirectory "%ROOT%"                               || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppStdout "%LOG_DIR%\llm-gateway.log"               || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppStderr "%LOG_DIR%\llm-gateway.err.log"           || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppRotateFiles 1                                    || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppRotateBytes 10485760                             || goto nssm_fail
rem Restart the gateway whenever it exits, and give the 10s request drain
rem time to finish before NSSM escalates past the console stop signal.
"%NSSM%" set "%SERVICE_NAME%" AppExit Default Restart                             || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppRestartDelay 5000                                || goto nssm_fail
"%NSSM%" set "%SERVICE_NAME%" AppStopMethodConsole 15000                          || goto nssm_fail

if exist "%USERPROFILE%\.config\llm-gateway\llm-gateway.pid" (
    echo WARNING: %USERPROFILE%\.config\llm-gateway\llm-gateway.pid exists.
    echo          A --daemon gateway may already be running; if it holds the
    echo          same listen address the service will fail to bind. Stop it
    echo          first with:  llm-gateway --stop
)

sc start "%SERVICE_NAME%" >nul
if errorlevel 1 (
    echo ERROR: the service was installed but failed to start. Check:
    sc query "%SERVICE_NAME%"
    exit /b 1
)

echo.
echo Service "%SERVICE_NAME%" installed and started.
if defined CONFIG_PATH (
    echo   Config : %CONFIG_PATH%
) else (
    echo   Config : default path - pass one explicitly to avoid surprises
)
echo   Logs   : %LOG_DIR%\llm-gateway.log
echo   Status : %~nx0 status
echo   Remove : %~nx0 uninstall
exit /b 0

:nssm_fail
echo ERROR: configuring the service with NSSM failed; rolling back.
"%NSSM%" remove "%SERVICE_NAME%" confirm >nul 2>&1
exit /b 1

:uninstall
sc query "%SERVICE_NAME%" >nul 2>&1
if errorlevel 1 (
    echo Service "%SERVICE_NAME%" is not installed.
    exit /b 0
)
echo Stopping "%SERVICE_NAME%"...
sc stop "%SERVICE_NAME%" >nul 2>&1
rem Wait up to 20s so the 10s request drain finishes before deletion.
powershell -NoProfile -Command "$s = Get-Service -Name '%SERVICE_NAME%' -ErrorAction SilentlyContinue; if ($s -and $s.Status -ne 'Stopped') { try { $s.WaitForStatus('Stopped', (New-TimeSpan -Seconds 20)) } catch {} }" >nul 2>&1
sc delete "%SERVICE_NAME%" >nul
if errorlevel 1 (
    echo ERROR: sc delete failed. Inspect "%SERVICE_NAME%" in services.msc.
    exit /b 1
)
echo Service "%SERVICE_NAME%" removed. Log files in %LOG_DIR% were kept.
exit /b 0

:svc_start
sc query "%SERVICE_NAME%" >nul 2>&1
if errorlevel 1 (
    echo Service "%SERVICE_NAME%" is not installed. Run:  %~nx0 install
    exit /b 1
)
sc start "%SERVICE_NAME%"
exit /b

:svc_stop
sc query "%SERVICE_NAME%" >nul 2>&1
if errorlevel 1 (
    echo Service "%SERVICE_NAME%" is not installed. Run:  %~nx0 install
    exit /b 1
)
sc stop "%SERVICE_NAME%"
exit /b

:status
sc query "%SERVICE_NAME%" >nul 2>&1
if errorlevel 1 (
    echo Service "%SERVICE_NAME%" is not installed. Run:  %~nx0 install
    exit /b 1
)
sc query "%SERVICE_NAME%"
exit /b 0

:usage
echo Usage: %~nx0 install [config-path]
echo        %~nx0 uninstall ^| start ^| stop ^| status
echo.
echo   install    Register llm-gateway as an auto-start Windows service
echo              ^(NSSM wrapper^), start it immediately. Accepts an optional
echo              config path; without one, config.yaml in the repo root or
echo              the user default path is used if it exists.
echo   uninstall  Stop and remove the service ^(logs are kept^).
echo   start      Start the service.
echo   stop       Stop the service ^(in-flight requests get the 10s drain^).
echo   status     Show the service status. The only command that needs no
echo              administrator rights.
echo.
echo Logs are written to %LOG_DIR%\.
exit /b 1

:ensure_nssm
if exist "%NSSM%" exit /b 0
where nssm >nul 2>&1
if not errorlevel 1 (
    set "NSSM=nssm"
    exit /b 0
)
echo NSSM not found - downloading %NSSM_URL% ...
set "NSSM_ZIP=%TEMP%\llm-gateway-nssm.zip"
set "NSSM_TMP=%TEMP%\llm-gateway-nssm"
powershell -NoProfile -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; $ProgressPreference = 'SilentlyContinue'; Invoke-WebRequest -UseBasicParsing -Uri '%NSSM_URL%' -OutFile '%NSSM_ZIP%'; if (Test-Path '%NSSM_TMP%') { Remove-Item -Recurse -Force '%NSSM_TMP%' }; Expand-Archive -Path '%NSSM_ZIP%' -DestinationPath '%NSSM_TMP%' -Force"
if errorlevel 1 (
    echo ERROR: failed to download or extract NSSM.
    echo        Download %NSSM_URL% manually, extract it, and copy
    echo        nssm.exe to %NSSM%, then re-run this script.
    exit /b 1
)
set "NSSM_ARCH=win64"
if /i "%PROCESSOR_ARCHITECTURE%"=="x86" if not defined PROCESSOR_ARCHITEW6432 set "NSSM_ARCH=win32"
copy /y "%NSSM_TMP%\nssm-%NSSM_VERSION%\%NSSM_ARCH%\nssm.exe" "%NSSM%" >nul
if not exist "%NSSM%" (
    echo ERROR: could not extract nssm.exe from the downloaded archive.
    echo        Copy it manually to %NSSM% and re-run this script.
    exit /b 1
)
echo NSSM installed to %NSSM%
exit /b 0
