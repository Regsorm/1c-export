@echo off
REM ============================================================================
REM  1c-export: обёртка с автоматической самоэлевацией (запуск от администратора)
REM
REM  Назначение:
REM    Запускает 1c-export.exe с правами администратора. Это обязательно, когда
REM    используется доменная (Windows-интегрированная) авторизация MSSQL —
REM    доступ к SPN/Kerberos и ACL целевых папок (напр. C:\Repos\...) требует
REM    повышенных прав.
REM
REM  Как работает:
REM    1) Проверяет, запущен ли скрипт от администратора.
REM    2) Если нет — перезапускает сам себя через PowerShell Start-Process -Verb RunAs
REM       (покажется UAC-подтверждение).
REM    3) Если да — запускает 1c-export.exe, передавая все аргументы.
REM
REM  Использование:
REM    run-1c-export.bat --config config/config.json --export-base ^
REM                      --export-extensions --ibcmd-db-auth-windows ^
REM                      --ibcmd-sync --ibcmd-incremental
REM
REM    Можно также создать ярлык на этот bat и ставить галку
REM    "Run as administrator" в свойствах ярлыка (Advanced) — тогда
REM    UAC спросит подтверждение ещё до запуска cmd.
REM ============================================================================

setlocal EnableExtensions

REM --- Путь к бинарнику 1c-export.exe (правьте под свою сборку) -----------------
set "EXE=%~dp0..\target\release\1c-export.exe"
if not exist "%EXE%" set "EXE=%~dp01c-export.exe"

if not exist "%EXE%" (
    echo [ERROR] 1c-export.exe не найден.
    echo Пробовал: %~dp0..\target\release\1c-export.exe
    echo          %~dp01c-export.exe
    echo Соберите проект: cargo build --release
    pause
    exit /b 1
)

REM --- Проверка: запущены ли мы с правами администратора ------------------------
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo Требуются права администратора. Перезапуск через UAC...
    REM Собираем исходные аргументы обратно в одну строку и передаём их в новый cmd
    powershell -NoProfile -Command ^
        "Start-Process -FilePath 'cmd.exe' -ArgumentList '/c \"\"%~f0\" %*\"' -Verb RunAs"
    exit /b
)

REM --- Уже администратор — запускаем реальную выгрузку --------------------------
echo [INFO] Запуск от администратора: %EXE%
"%EXE%" %*
set "RC=%errorlevel%"

REM Если bat запущен двойным кликом — оставляем окно открытым
echo.
echo [INFO] Код возврата: %RC%
if "%1"=="" pause
exit /b %RC%
