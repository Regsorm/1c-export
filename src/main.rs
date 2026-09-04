// Скрыть консольное окно в release-сборке. В CLI/watch-режимах stdout всё равно
// перенаправляется в файл (см. Logger::init_file_in), так что потери видимости нет.
// В debug-сборке консоль остаётся — удобно для разработки.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bases_config;
mod command_builder;
mod config;
mod error;
mod eventlog_watcher;
mod export;
mod flush;
mod git_push;
mod gui;
mod logging;
mod mcp_client;
mod processings;
mod runner;
mod sql_discovery;
mod sql_signals;
mod state;
mod state_db;
mod storage_mapping;
#[allow(dead_code, unused_imports)]
mod v8container;
mod watch;

use clap::{Parser, Subcommand, ValueEnum};
use config::AppConfig;
use logging::Logger;
use std::path::Path;
use std::process;

/// Тип аутентификации для CLI
#[derive(Debug, Clone, ValueEnum)]
enum AuthTypeArg {
    Os,
    Password,
}

/// Тип git-аутентификации для --git-push
#[derive(Debug, Clone, ValueEnum, Default)]
enum GitAuthArg {
    /// Доменная / системная (Credential Manager / SSH-агент / git credential helper)
    #[default]
    Domain,
    /// Явные --git-user / --git-password подставляются в URL remote на лету
    Password,
}

/// Подкоманды.
#[derive(Subcommand, Debug)]
enum Commands {
    /// Watch-режим: периодический опрос служебных таблиц SQL или журнала регистрации
    /// баз и автоматическая выгрузка при обнаружении изменений конфигурации/расширения.
    /// См. README.md, раздел «Режим watch».
    Watch {
        /// Путь к bases.json (см. README.md, раздел «Режим watch»)
        #[arg(long)]
        bases: String,

        /// Запустить только один цикл и выйти (для smoke-тестов и Scheduled Task'ов
        /// с внешним расписанием).
        #[arg(long)]
        once: bool,
    },
}

/// CLI для выгрузки конфигурации 1С через IBCMD
#[derive(Parser, Debug)]
#[command(name = "1c-export")]
#[command(version = "3.0.0")]
#[command(about = "Выгрузка конфигурации 1С через IBCMD (многопоточно, c поддержкой --sync)")]
#[command(after_help = "Примеры:\n  1c-export --config config/config.json --export-base --export-extensions\n  1c-export --config config/config.json --export-base --ibcmd-db-auth-windows --ibcmd-sync\n  1c-export --server sql-server --database demo-ut --auth-type password --login export_user --password <пароль> --export-base --ibcmd-db-auth-windows --ibcmd-sync --ibcmd-jobs 8 --output-path C:\\Repos\\demo-ut\n  1c-export watch --bases C:\\1c-export-daemon\\bases.json")]
struct Cli {
    /// Подкоманда. Если не указана — выполняется разовая выгрузка по флагам.
    #[command(subcommand)]
    command: Option<Commands>,

    /// Путь к файлу конфигурации JSON
    #[arg(long)]
    config: Option<String>,

    // --- Подключение ---
    /// Сервер MSSQL (для SQL-коннекта при --export-processings, а также default для /S если
    /// --server-1c не задан).
    #[arg(long)]
    server: Option<String>,

    /// Сервер кластера 1С (для ENTERPRISE/DESIGNER /S и IBCMD --ibconnection). Если не
    /// указан — используется --server. Укажите отдельно, когда кластер 1С и СУБД живут
    /// на разных машинах.
    #[arg(long)]
    server_1c: Option<String>,

    /// Имя базы данных
    #[arg(long)]
    database: Option<String>,

    /// Тип аутентификации в информационной базе 1С: os (Windows) или password (1С-логин)
    #[arg(long, value_enum)]
    auth_type: Option<AuthTypeArg>,

    /// Логин пользователя 1С (для --auth-type password)
    #[arg(long)]
    login: Option<String>,

    /// Пароль пользователя 1С (для --auth-type password)
    #[arg(long)]
    password: Option<String>,

    /// Путь к ibcmd.exe (полный путь до файла). 1cv8.exe не используется в watch-пути,
    /// поэтому отдельно его указывать не нужно.
    #[arg(long)]
    ibcmd_path: Option<String>,

    /// Папка для выгрузки результатов
    #[arg(long)]
    output_path: Option<String>,

    // --- Что выгружать ---
    /// Выгрузить основную конфигурацию
    #[arg(long)]
    export_base: bool,

    /// Выгрузить все расширения
    #[arg(long)]
    export_extensions: bool,

    /// Выгрузить справочник «ДополнительныеОтчетыИОбработки» (БСП) напрямую из MSSQL,
    /// минуя Designer. Ожидает установленного расширения `ВыгрузкаВсехВнешнихОбработок`
    /// в ИБ (для автодискавери имён таблицы/полей) и валидных SQL-кредов
    /// (--ibcmd-db-auth-windows или --ibcmd-db-user/--ibcmd-db-pwd).
    #[arg(long)]
    export_processings: bool,

    /// Сохранять бинарные снимки `_artifacts/base.cf` и `_artifacts/extensions/<имя>.cfe`
    /// через `ibcmd config save`. По умолчанию выключено: снимки большие и нужны только
    /// для развёртывания на стенд через `ibcmd config load`. Включается также полем
    /// `saveArtifacts` в config.json / bases.json.
    #[arg(long)]
    save_artifacts: bool,

    // --- SQL-параметры выгрузки допобработок ---
    /// Override MSSQL-сервера (если отличается от имени 1С-сервера из --server).
    #[arg(long)]
    sql_server: Option<String>,

    /// Явное имя таблицы справочника ДополнительныеОтчетыИОбработки (`_Reference...`).
    /// Обычно определяется автоматически через расширение, флаг — для отладки/fallback.
    #[arg(long)]
    processings_table: Option<String>,

    /// Явное имя поля ХранилищеОбработки (`_Fld...`).
    #[arg(long)]
    processings_field_storage: Option<String>,

    /// Явное имя поля КонтрольнаяСумма (`_Fld...`).
    #[arg(long)]
    processings_field_hash: Option<String>,

    /// Явное имя поля Вид (`_Fld...`).
    #[arg(long)]
    processings_field_kind: Option<String>,

    /// Форсировать повторный запуск автодискавери структуры хранения
    /// (игнорировать кэш в `_manifest.json`).
    #[arg(long)]
    rediscover: bool,

    /// Источник SQL-имён таблицы и полей справочника допобработок:
    /// sql = напрямую по служебным таблицам MS SQL (Params/Config),
    /// mcp = HTTP-сервис внутри базы,
    /// auto = сначала sql, при ошибке — mcp (по умолчанию).
    #[arg(long, value_enum, default_value = "auto")]
    discovery: export::DiscoveryMode,

    // --- IBCMD-параметры ---
    /// Тип СУБД (по умолчанию MSSQLServer)
    #[arg(long, default_value = "MSSQLServer")]
    ibcmd_dbms: String,

    /// Доменная (Windows-интегрированная) авторизация MSSQL — --db-user/--db-pwd не передаются.
    /// Если не указано — используется SQL-логин (--ibcmd-db-user/--ibcmd-db-pwd).
    #[arg(long)]
    ibcmd_db_auth_windows: bool,

    /// Пользователь БД (SQL-авторизация, прямое подключение к СУБД)
    #[arg(long)]
    ibcmd_db_user: Option<String>,

    /// Пароль БД (SQL-авторизация, прямое подключение к СУБД)
    #[arg(long)]
    ibcmd_db_pwd: Option<String>,

    /// Использовать --ibconnection=Srvr=..;Ref=.. вместо прямого подключения к СУБД
    #[arg(long)]
    ibcmd_use_connection_string: bool,

    /// Количество потоков IBCMD (0 = автоматически)
    #[arg(long, default_value = "0")]
    ibcmd_jobs: u32,

    /// Инкрементальная синхронизация XML-дампа (--sync).
    /// Применяется ТОЛЬКО к основной конфигурации. Расширения всегда
    /// выгружаются полностью (одной командой `config export all-extensions`).
    #[arg(long)]
    ibcmd_sync: bool,

    /// Флаг --force для ibcmd. Использовать с осторожностью.
    /// С --sync: при несовпадении формата ConfigDumpInfo.xml делает полный дамп вместо падения.
    /// Без --sync: потенциально позволяет писать в непустую папку.
    #[arg(long)]
    ibcmd_force: bool,

    /// Инкрементальная выгрузка расширений: по hash-sum из `config extension list`,
    /// сравнение с прошлым запуском через `git show HEAD:.extensions-hashes.json`.
    /// Выгружаются только изменившиеся/новые расширения, удалённые из ИБ — удаляются из папки.
    #[arg(long)]
    ibcmd_incremental: bool,

    /// Полная перезапись допобработок (по умолчанию инкремент): чистит External/ целиком.
    #[arg(long)]
    processings_full: bool,

    /// Подробный вывод
    #[arg(long)]
    verbose: bool,

    // --- git push после успешной выгрузки ---
    /// После успеха сделать `git add -A && git commit && git push` в каталоге --git-repo
    /// (или --output-path, если --git-repo не задан). При ошибке push — exit code 2.
    #[arg(long)]
    git_push: bool,

    /// Каталог git-репо для коммита. Если не указан — используется --output-path.
    #[arg(long)]
    git_repo: Option<String>,

    /// Способ git-аутентификации. domain = Credential Manager / git helper (default).
    /// password = подстановка --git-user/--git-password в URL remote.
    #[arg(long, value_enum, default_value_t = GitAuthArg::Domain)]
    git_auth_type: GitAuthArg,

    /// Логин для --git-auth-type=password.
    #[arg(long)]
    git_user: Option<String>,

    /// Пароль для --git-auth-type=password. URL-кодирование спецсимволов делается автоматически.
    #[arg(long)]
    git_password: Option<String>,

    /// Сообщение коммита. По умолчанию "Update_yyyyMMdd".
    #[arg(long)]
    git_message: Option<String>,
}

fn main() {
    if std::env::args().len() <= 1 {
        gui::run_gui();
        return;
    }
    let cli = Cli::parse();

    // Подкоманды (например, watch) — отдельная ветка с собственным tokio-runtime.
    if let Some(cmd) = cli.command {
        run_subcommand(cmd);
        return;
    }
    run_cli(cli);
}

/// Подкоманды watch-режима. Поднимает tokio multi-thread runtime и делегирует.
fn run_subcommand(cmd: Commands) {
    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| {
        eprintln!("ОШИБКА: не удалось создать tokio runtime: {}", e);
        process::exit(1);
    });
    match cmd {
        Commands::Watch { bases, once } => {
            // Single-instance lock: под Windows — Named Mutex. Если другой
            // 1c-export.exe watch уже крутится в этой же сессии — просто выходим.
            // _instance держим до конца ветки, чтобы Drop снял мьютекс на выходе.
            let instance = single_instance::SingleInstance::new("1c-export-watch-mutex")
                .unwrap_or_else(|e| {
                    eprintln!("watch: не удалось создать single-instance lock: {}", e);
                    process::exit(1);
                });
            if !instance.is_single() {
                eprintln!(
                    "watch: уже запущен другой экземпляр 1c-export watch. \
                     Параллельный запуск запрещён — выход."
                );
                process::exit(1);
            }

            // 1. Загрузка bases.json — синхронно, до открытия файл-лога.
            //    Если упало — log_dir неизвестен, ошибка идёт в stderr, выход.
            let bases_path = Path::new(&bases);
            let mut cfg = match bases_config::DaemonConfig::load(bases_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("watch: не удалось загрузить bases.json: {:#}", e);
                    process::exit(1);
                }
            };

            // 2. Резолв относительных путей. Все пути в bases.json (state_dir,
            //    log_dir, config_path каждой базы) резолвятся относительно
            //    каталога самого bases.json — чтобы пакет лежал одной самодостаточной
            //    папкой и не зависел от cwd процесса.
            let bases_dir = bases_path.parent().unwrap_or(Path::new("."));
            fn resolve(base: &Path, raw: &str) -> String {
                let p = std::path::PathBuf::from(raw);
                if p.is_absolute() { raw.to_string() } else { base.join(p).to_string_lossy().into_owned() }
            }
            cfg.state_dir = resolve(bases_dir, &cfg.state_dir);
            cfg.log_dir = resolve(bases_dir, &cfg.log_dir);
            // bases[*].output_path и ibcmd_path в шаблонах абсолютные; если оператор
            // оставит относительные — резолвим тоже относительно каталога bases.json.
            for b in cfg.bases.iter_mut() {
                b.output_path = resolve(bases_dir, &b.output_path);
                b.ibcmd_path = resolve(bases_dir, &b.ibcmd_path);
            }

            // 3. Инициализация файл-лога — log_dir уже абсолютный после resolve.
            //    Уровень подробности берём из bases.json до первых строк журнала.
            logging::Logger::set_level(logging::LogLevel::parse(&cfg.log_level));
            let log_dir = std::path::PathBuf::from(&cfg.log_dir);
            logging::Logger::init_file_in(log_dir);
            logging::Logger::install_panic_hook();

            // 3. Async-часть: цикл watch.
            let result = rt.block_on(async move { watch::run(cfg, once).await });
            // instance дропается здесь — мьютекс освобождается.
            drop(instance);
            if let Err(e) = result {
                // Дублируем в файл-лог (если открыт) и в stderr — оператор увидит
                // фатальную ошибку и в watch-YYYY-MM-DD.log, и в консольном выводе службы.
                logging::Logger::log(&format!("watch: ФАТАЛЬНАЯ ОШИБКА: {:#}", e));
                eprintln!("watch: ФАТАЛЬНАЯ ОШИБКА: {:#}", e);
                process::exit(1);
            }
        }
    }
}

fn run_cli(cli: Cli) {
    if !(cli.export_base || cli.export_extensions || cli.export_processings) {
        eprintln!("ОШИБКА: Не выбрано ни одного действия для выгрузки");
        eprintln!(
            "Используйте --export-base и/или --export-extensions и/или --export-processings"
        );
        eprintln!("Для справки: --help");
        process::exit(1);
    }

    // Загрузка конфигурации
    let mut app_config = match &cli.config {
        Some(path) => AppConfig::load(Path::new(path)),
        None => AppConfig::load_auto(),
    }
    .unwrap_or_else(|e| {
        eprintln!("ОШИБКА: {}", e);
        process::exit(1);
    });

    // Уровень подробности журнала — из config.json.
    Logger::set_level(logging::LogLevel::parse(&app_config.log_level));

    // CLI -> config
    if let Some(ref server) = cli.server { app_config.server = server.clone(); }
    if let Some(ref s1c) = cli.server_1c { app_config.server_1c = s1c.clone(); }
    if let Some(ref database) = cli.database { app_config.database = database.clone(); }
    if let Some(ref auth_type) = cli.auth_type {
        app_config.authentication.auth_type = match auth_type {
            AuthTypeArg::Os => config::AuthType::Os,
            AuthTypeArg::Password => config::AuthType::Password,
        };
    }
    if let Some(ref login) = cli.login { app_config.authentication.login = login.clone(); }
    if let Some(ref password) = cli.password { app_config.authentication.password = password.clone(); }
    if let Some(ref ibcmd_path) = cli.ibcmd_path { app_config.ibcmd_path = ibcmd_path.clone(); }
    if let Some(ref output_path) = cli.output_path { app_config.output_path = output_path.clone(); }

    if let Err(errors) = app_config.validate() {
        eprintln!("ОШИБКА: Неверная конфигурация:");
        for err in &errors {
            eprintln!("  - {}", err);
        }
        process::exit(1);
    }

    if cli.verbose {
        println!("{}", "=".repeat(60));
        println!("КОНФИГУРАЦИЯ ВЫГРУЗКИ");
        println!("{}", "=".repeat(60));
        println!("Сервер MSSQL:   {}", app_config.server);
        println!("Сервер 1С:      {}{}",
            app_config.server_for_1c(),
            if app_config.server_1c.is_empty() { " (= MSSQL, явно не задан)" } else { "" });
        println!("База данных:    {}", app_config.database);
        println!("Аутентификация 1С: {:?}", app_config.authentication.auth_type);
        println!("ibcmd.exe:      {}", app_config.ibcmd_path);
        println!("Выгрузка в:     {}", app_config.output_path);
        println!("Инкрементально: {}", cli.ibcmd_sync);
        println!("{}", "=".repeat(60));
    }

    // Параметры IBCMD
    let ibcmd_path = match app_config.ibcmd_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ОШИБКА: {}", e);
            process::exit(1);
        }
    };
    if cli.verbose {
        Logger::log(&format!("IBCMD найден: {}", ibcmd_path.display()));
    }
    let db_auth = if cli.ibcmd_db_auth_windows {
        command_builder::IbcmdDbAuth::Windows
    } else {
        command_builder::IbcmdDbAuth::SqlLogin
    };
    let ibcmd_params = command_builder::IbcmdParams {
        ibcmd_path,
        dbms: cli.ibcmd_dbms.clone(),
        db_auth,
        db_user: cli.ibcmd_db_user.clone(),
        db_pwd: cli.ibcmd_db_pwd.clone(),
        use_connection_string: cli.ibcmd_use_connection_string,
        jobs: cli.ibcmd_jobs,
        sync: cli.ibcmd_sync,
        force: cli.ibcmd_force,
        incremental_extensions: cli.ibcmd_incremental,
    };

    // Параметры выгрузки допобработок (только если выбран режим --export-processings).
    let processings_params = if cli.export_processings {
        // Валидация SQL-кредов — без них SQL-коннект заведомо не поднимется.
        if !cli.ibcmd_db_auth_windows
            && (cli.ibcmd_db_user.is_none() || cli.ibcmd_db_pwd.is_none())
        {
            eprintln!(
                "ОШИБКА: --export-processings требует либо --ibcmd-db-auth-windows, \
                 либо связки --ibcmd-db-user/--ibcmd-db-pwd (SQL-коннект)"
            );
            process::exit(1);
        }

        // Override имён таблицы/полей из CLI (если указан хотя бы один — требуем все четыре).
        let override_mapping = match (
            &cli.processings_table,
            &cli.processings_field_storage,
            &cli.processings_field_hash,
            &cli.processings_field_kind,
        ) {
            (Some(table), Some(storage), Some(hash), Some(kind)) => {
                Some(processings::StorageMapping {
                    table: table.clone(),
                    field_storage: storage.clone(),
                    field_hash: hash.clone(),
                    field_kind: kind.clone(),
                    // CLI override не покрывает таблицу перечисления видов —
                    // карта видов останется пустой, файлы уйдут как .epf.
                    enum_table: String::new(),
                    // CLI override: если имя содержит "_Version" или "Version" —
                    // считаем бинарным rowversion, иначе строкой (MD5).
                    hash_is_binary: hash.eq_ignore_ascii_case("_Version")
                        || hash.to_lowercase().contains("version"),
                })
            }
            (None, None, None, None) => None,
            _ => {
                eprintln!(
                    "ОШИБКА: --processings-table/--processings-field-* указываются все вместе или ни одного"
                );
                process::exit(1);
            }
        };

        Some(export::ProcessingsCliParams {
            sql_server: cli
                .sql_server
                .clone()
                .unwrap_or_else(|| app_config.server.clone()),
            override_mapping,
            rediscover: cli.rediscover,
            incremental: !cli.processings_full,
            discovery: cli.discovery,
        })
    } else {
        None
    };

    // Сохраняем output_path до move app_config в координатор — нужен для --git-repo fallback.
    let output_path_for_git = app_config.output_path.clone();
    // Настройки git из config.json (core.autocrlf) — тоже до move app_config.
    let git_opts = git_push::GitOptions::new(&app_config.git_autocrlf);
    // Бинарные снимки включает либо ключ CLI, либо поле saveArtifacts в config.json.
    let save_artifacts = cli.save_artifacts || app_config.save_artifacts;
    // Идентификатор базы в state.db (журнал выгрузок И состояние инкремента):
    // alias базы из bases.json по каталогу выгрузки — тот же ключ, что у службы
    // watch. Базы нет в реестре — имя последней папки output_path, как раньше.
    let repo_id_for_log = bases_config::alias_for_output_path(&output_path_for_git)
        .unwrap_or_else(|| export::ExportCoordinator::derive_repo_id(&output_path_for_git));

    let coordinator =
        export::ExportCoordinator::new(app_config).with_repo_id(repo_id_for_log.as_str());
    let opts = export::ExportOptions {
        export_base: cli.export_base,
        export_extensions: cli.export_extensions,
        export_processings: cli.export_processings,
        save_artifacts,
        // Ручной запуск: менялась ли конфигурация, неизвестно — снимок пишем как раньше.
        config_changed: None,
        ibcmd_params,
        processings_params,
    };

    Logger::log("Запуск выгрузки...");
    let started = std::time::Instant::now();
    let results = coordinator.export_full(&opts);

    let success = results.overall_success();
    // Журнал выгрузок в state.db рядом с exe — его показывает вкладка «История» GUI.
    export::record_export_log(&repo_id_for_log, &results, started.elapsed().as_secs());

    if success {
        Logger::log("✓ Выгрузка завершена успешно");
        // git push — только при успешной выгрузке.
        if cli.git_push {
            let repo_path = cli
                .git_repo
                .as_deref()
                .unwrap_or(&output_path_for_git);
            let auth = match cli.git_auth_type {
                GitAuthArg::Domain => git_push::GitAuth::Domain,
                GitAuthArg::Password => git_push::GitAuth::UserPassword {
                    user: cli.git_user.clone().unwrap_or_default(),
                    password: cli.git_password.clone().unwrap_or_default(),
                },
            };
            let message = cli
                .git_message
                .clone()
                .unwrap_or_else(git_push::default_commit_message);
            match git_push::commit_and_push(Path::new(repo_path), &message, &auth, &git_opts) {
                Ok(_) => {
                    Logger::log("✓ git push прошёл");
                }
                Err(e) => {
                    Logger::log(&format!("✗ git push не удался: {}", e));
                    process::exit(2);
                }
            }
        }

        // Phase 6: безусловное удаление external/processings/ — кэш сырых
        // .epf-бинарей не должен жить дольше успешного цикла.
        export::force_remove_processings_cache(Path::new(&output_path_for_git));
    } else {
        Logger::log("✗ Выгрузка завершена с ошибками");
        process::exit(1);
    }
}
