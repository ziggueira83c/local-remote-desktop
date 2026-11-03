@echo off
setlocal EnableExtensions EnableDelayedExpansion
title Instalador - Local Remote Desktop

rem ============================================================
rem  Instalador em 1 clique do Local Remote Desktop (Windows 11)
rem  1. Instala as ferramentas (Visual Studio Build Tools C++,
rem     Rust e Node.js) se ainda nao existirem
rem  2. Compila o programa
rem  3. Instala o programa e libera a porta no firewall
rem  Registros: instalacao-log.txt e compilacao-log.txt
rem ============================================================

rem --- Pede permissao de administrador (Build Tools e firewall precisam) ---
net session >nul 2>&1
if errorlevel 1 (
    echo Pedindo permissao de administrador...
    powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)

set "ROOT=%~dp0"
set "LOG=%ROOT%instalacao-log.txt"
set "BUILDLOG=%ROOT%compilacao-log.txt"

if exist "%ROOT%apps\desktop\package.json" (
    set "PROJ=%ROOT%"
) else if exist "%ROOT%local-remote-desktop\apps\desktop\package.json" (
    set "PROJ=%ROOT%local-remote-desktop\"
) else (
    echo.
    echo [ERRO] Nao encontrei o codigo do programa.
    echo Deixe este arquivo na pasta que tem as pastas "apps" e "crates".
    pause
    exit /b 1
)

echo Instalador do Local Remote Desktop - %date% %time% > "%LOG%"
if exist "%BUILDLOG%" del "%BUILDLOG%"
echo.
echo ============================================================
echo   LOCAL REMOTE DESKTOP - INSTALACAO
echo ============================================================
echo  Na primeira vez pode levar de 20 a 40 minutos:
echo  baixa ferramentas de desenvolvimento e compila o programa.
echo  Nao feche esta janela.
echo ============================================================
echo.

where winget >nul 2>&1
if errorlevel 1 (
    echo [ERRO] O winget nao foi encontrado.
    echo Abra a Microsoft Store, procure "Instalador de Aplicativo" e atualize. Depois rode este arquivo de novo.
    pause
    exit /b 1
)

set "WG=--exact --silent --accept-source-agreements --accept-package-agreements"

rem --- 1. Visual Studio Build Tools com C++ ---
echo [1/6] Verificando Visual Studio Build Tools - C++...
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
set "HASVC="
if exist "!VSWHERE!" (
    for /f "usebackq delims=" %%i in (`"!VSWHERE!" -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "HASVC=%%i"
)
if defined HASVC (
    echo       Ja instalado.
) else (
    echo       Instalando - sao varios GB, pode demorar bastante...
    winget install --id Microsoft.VisualStudio.2022.BuildTools %WG% --override "--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended" >> "%LOG%" 2>&1
    set "HASVC="
    if exist "!VSWHERE!" (
        for /f "usebackq delims=" %%i in (`"!VSWHERE!" -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "HASVC=%%i"
    )
    if not defined HASVC (
        echo [ERRO] Nao foi possivel instalar o Visual Studio Build Tools.
        goto :fail
    )
)

rem --- 2. Rust ---
echo [2/6] Verificando Rust...
set "PATH=%USERPROFILE%\.cargo\bin;%ProgramFiles%\nodejs;%PATH%"
where cargo >nul 2>&1
if errorlevel 1 (
    echo       Instalando Rust...
    winget install --id Rustlang.Rustup %WG% >> "%LOG%" 2>&1
    set "PATH=%USERPROFILE%\.cargo\bin;!PATH!"
)
where rustup >nul 2>&1
if errorlevel 1 (
    echo [ERRO] O Rust nao foi instalado corretamente.
    goto :fail
)
call rustup default stable-x86_64-pc-windows-msvc >> "%LOG%" 2>&1
where cargo >nul 2>&1
if errorlevel 1 (
    echo [ERRO] O cargo nao esta disponivel.
    goto :fail
)
echo       OK.

rem --- 3. Node.js ---
echo [3/6] Verificando Node.js...
where npm >nul 2>&1
if errorlevel 1 (
    echo       Instalando Node.js LTS...
    winget install --id OpenJS.NodeJS.LTS %WG% >> "%LOG%" 2>&1
    set "PATH=%ProgramFiles%\nodejs;!PATH!"
)
where npm >nul 2>&1
if errorlevel 1 (
    echo [ERRO] O Node.js nao foi instalado corretamente.
    goto :fail
)
echo       OK.

rem --- 4. Dependencias da interface ---
echo [4/6] Baixando dependencias da interface...
cd /d "%PROJ%apps\desktop"
call :run npm install --no-fund --no-audit
if errorlevel 1 goto :fail

rem --- 5. Compilacao ---
echo.
echo [5/6] Compilando o programa - esta e a parte mais demorada...
call :run npm run tauri build -- --bundles nsis
if errorlevel 1 goto :fail

set "SETUP="
for %%f in ("%PROJ%target\release\bundle\nsis\*-setup.exe") do set "SETUP=%%~ff"
if not defined SETUP (
    echo [ERRO] A compilacao terminou, mas o instalador nao foi encontrado.
    goto :fail
)

rem --- 6. Instalacao e firewall ---
echo.
echo [6/6] Instalando o programa e liberando o firewall...
"%SETUP%" /S
if errorlevel 1 (
    echo [ERRO] O instalador retornou erro.
    goto :fail
)

netsh advfirewall firewall delete rule name="Local Remote Desktop (QUIC)" >nul 2>&1
netsh advfirewall firewall delete rule name="Local Remote Desktop (mDNS)" >nul 2>&1
netsh advfirewall firewall add rule name="Local Remote Desktop (QUIC)" dir=in action=allow protocol=UDP localport=47321 profile=private >> "%LOG%" 2>&1
netsh advfirewall firewall add rule name="Local Remote Desktop (mDNS)" dir=in action=allow protocol=UDP localport=5353 profile=private >> "%LOG%" 2>&1

copy /y "%SETUP%" "%ROOT%LocalRemoteDesktop-Setup.exe" >nul 2>&1

echo.
echo ============================================================
echo   PRONTO! O Local Remote Desktop foi instalado.
echo ============================================================
echo  - Abra pelo Menu Iniciar: "Local Remote Desktop".
echo  - Para instalar em outro PC Windows sem compilar de novo,
echo    use o arquivo LocalRemoteDesktop-Setup.exe desta pasta.
echo  - A rede do Windows precisa estar como "Privada".
echo.

rem Abre o programa sem privilegios de administrador.
set "APPEXE="
for %%d in ("%LOCALAPPDATA%\Local Remote Desktop" "%ProgramFiles%\Local Remote Desktop") do (
    for %%e in ("%%~d\*.exe") do (
        if /i not "%%~ne"=="uninstall" if not defined APPEXE set "APPEXE=%%~fe"
    )
)
if defined APPEXE (
    explorer.exe "!APPEXE!"
)
pause
exit /b 0

rem ------------------------------------------------------------
rem Executa um comando mostrando a saida na tela e gravando no log
:run
echo   ^> %*
powershell -NoProfile -ExecutionPolicy Bypass -Command "cmd /c '%* 2>&1' | Tee-Object -FilePath $env:BUILDLOG -Append; exit $LASTEXITCODE"
exit /b %errorlevel%

:fail
echo.
echo ============================================================
echo   A INSTALACAO NAO TERMINOU.
echo ============================================================
echo  Os registros estao nesta pasta:
echo    compilacao-log.txt   - erros de compilacao
echo    instalacao-log.txt   - instalacao das ferramentas
echo  Envie esses arquivos para o Claude corrigir o problema.
echo.
pause
exit /b 1
