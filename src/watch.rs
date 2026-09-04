//! Watch-mode: периодический опрос нескольких баз (служебные таблицы MS SQL либо журнал
//! регистрации через HTTP-сервис базы), автоматическая выгрузка при обнаружении изменений
//! конфигурации/расширений/допобработок, commit + push в GitLab. Всё in-process.
//!
//! См. README.md, раздел «Режим watch».

use std::path::PathBuf;
use std::time::Instant;

use crate::bases_config::{BaseEntry, ChangeDetection, DaemonConfig};
use crate::command_builder::{IbcmdDbAuth, IbcmdParams};
use crate::config::AppConfig;
use crate::eventlog_watcher::{mark_events_processed, query_new_events, LogEvent};
use crate::export::{ExportCoordinator, ExportOptions, ProcessingsCliParams};
use crate::flush;
use crate::git_push::{self, GitAuth};
use crate::logging::Logger;
use crate::mcp_client::McpClient;
use crate::processings::{connect_mssql_raw, StorageMapping as ProcStorageMapping, TiberiusClient};
use crate::sql_signals::{diff_signals, take_signals, SignalScope, StoredMappingLite};
use crate::state::{BaseState, SqlSignals, StoredMapping};
use crate::storage_mapping::{fetch_enum_table, fetch_storage_mapping, StorageMapping};

/// Что запустило выгрузку в этом цикле — от этого зависит, что сдвигать в state.
enum Trigger {
    /// Режим eventlog: новые записи журнала регистрации.
    Events(Vec<LogEvent>),
    /// Режим sql: причины расхождения отпечатков и сами отпечатки, снятые ДО выгрузки.
    /// `config_changed` — сдвинулся ли отпечаток основной конфигурации: от него
    /// зависит, переписывать ли бинарный снимок `_artifacts/base.cf`.
    Signals { reasons: Vec<String>, signals: SqlSignals, config_changed: bool },
}

/// Главная точка входа watch-режима. Бесконечный цикл.
/// Если `once = true` — выполняется один цикл и возвращается.
pub async fn run(cfg: DaemonConfig, once: bool) -> anyhow::Result<()> {
    Logger::log(&format!(
        "watch: старт. баз={}, interval={}мин, once={}",
        cfg.bases.len(), cfg.check_interval_minutes, once
    ));

    loop {
        let cycle_start = chrono::Local::now();
        Logger::separator();
        Logger::log(&format!("=== ЦИКЛ старт: {} ===", cycle_start.format("%Y-%m-%d %H:%M:%S")));

        for base in &cfg.bases {
            let _ = process_one_base_safe(&cfg, base).await;
        }

        let cycle_dur = chrono::Local::now() - cycle_start;
        Logger::log(&format!(
            "=== ЦИКЛ завершён за {}с ===",
            cycle_dur.num_seconds()
        ));

        if once {
            return Ok(());
        }
        let sleep_secs = cfg.check_interval_minutes.saturating_mul(60);
        Logger::log(&format!("watch: sleep {}с (≈{}мин)", sleep_secs, cfg.check_interval_minutes));
        tokio::time::sleep(tokio::time::Duration::from_secs(sleep_secs)).await;
    }
}

/// Обработка одной базы с обработкой ошибок: исключение в одной базе не валит цикл.
/// Все ошибки журналируются и записываются в state.
/// MCP-клиент строится здесь per-base — каждый смотрит на свой `<base>/hs/mcp`.
async fn process_one_base_safe(cfg: &DaemonConfig, base: &BaseEntry) {
    let state_dir = PathBuf::from(&cfg.state_dir);
    let mut state = match BaseState::load(&state_dir, &base.alias) {
        Ok(s) => s,
        Err(e) => {
            Logger::log(&format!("[{}] ОШИБКА загрузки state: {:#}", base.alias, e));
            return;
        }
    };

    // Все настройки базы — в самой BaseEntry. Строим AppConfig из неё для совместимости
    // с ExportCoordinator/command_builder/processings, которые ожидают AppConfig.
    let app_config = crate::config::AppConfig::from_base(base);

    // HTTP-сервис базы нужен только режиму eventlog; режим sql ходит прямо в СУБД.
    let mcp = if base.change_detection == ChangeDetection::Eventlog {
        if base.mcp_url.trim().is_empty() {
            Logger::log(&format!("[{}] mcpUrl не задан — пропускаем", base.alias));
            return;
        }
        match McpClient::new(
            &base.mcp_url,
            &base.login,
            &base.password,
            &base.mcp_api_key,
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                Logger::log(&format!("[{}] не удалось создать McpClient: {:#}", base.alias, e));
                return;
            }
        }
    } else {
        None
    };

    match process_one_base(mcp.as_ref(), cfg, base, &app_config, &mut state).await {
        Ok(events_count) => {
            Logger::log(&format!("[{}] цикл завершён: обработано событий = {}", base.alias, events_count));
        }
        Err(e) => {
            Logger::log(&format!("[{}] ОШИБКА: {:#}", base.alias, e));
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            state.last_export_status = Some(format!("fail: {}", e));
            if let Ok(mut db) = crate::state_db::StateDb::open_default() {
                let _ = db.log_export(&crate::state_db::ExportLogEntry {
                    repo: base.alias.clone(),
                    finished_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    duration_sec: None,
                    status: "fail".to_string(),
                    events: None,
                    details: None,
                    error: Some(format!("{}", e)),
                });
            }
            if let Err(save_err) = state.save(&state_dir) {
                Logger::log(&format!("[{}] не удалось сохранить state: {:#}", base.alias, save_err));
            }
            if state.consecutive_failures >= 3 {
                Logger::log(&format!(
                    "[{}] ВНИМАНИЕ: {} неудач подряд. Требуется внимание оператора.",
                    base.alias, state.consecutive_failures
                ));
                // notify::alert(...) — заглушка, добавится позже
            }
        }
    }
}

/// Внутренний обработчик одной базы. Возвращает количество обработанных событий.
async fn process_one_base(
    mcp: Option<&McpClient>,
    cfg: &DaemonConfig,
    base: &BaseEntry,
    app_config: &crate::config::AppConfig,
    state: &mut BaseState,
) -> anyhow::Result<usize> {
    let state_dir = PathBuf::from(&cfg.state_dir);
    Logger::log(&format!(
        "[{}] {}",
        base.alias,
        match base.change_detection {
            ChangeDetection::Sql => "опрос служебных таблиц SQL...",
            ChangeDetection::Eventlog => "проверка журнала (HTTP-сервис)...",
        }
    ));

    // Режим sql: одно подключение к СУБД на цикл — и для определения структуры хранения,
    // и для снятия отпечатков. Закрывать не нужно, клиент живёт до конца функции.
    let mut sql_client: Option<TiberiusClient> = if base.change_detection == ChangeDetection::Sql {
        Some(connect_db(base).await?)
    } else {
        None
    };

    // 1. storage_mapping (если нужен для допобработок) — fetch при первом запуске или раз в N дней.
    if base.export_processings && state.needs_storage_refetch(cfg.refetch_storage_mapping_after_days) {
        // Имя справочника — глобальное на весь сервер (см. DaemonConfig.processings_meta_name).
        // Если в bases.json не задано — встроенный дефолт.
        let meta_name = if !cfg.processings_meta_name.is_empty() {
            cfg.processings_meta_name.as_str()
        } else {
            "Справочник.ДополнительныеОтчетыИОбработки"
        };
        let stored = match base.change_detection {
            ChangeDetection::Eventlog => {
                Logger::log(&format!("[{}] fetch_storage_mapping...", base.alias));
                let mcp = mcp.ok_or_else(|| anyhow::anyhow!("режим eventlog без HTTP-клиента"))?;
                let mapping = fetch_storage_mapping(mcp, meta_name).await?;
                Logger::log(&format!(
                    "[{}] mapping: table={} field_storage={} field_hash={} field_kind={} (binary_hash={})",
                    base.alias, mapping.table, mapping.field_storage, mapping.field_hash, mapping.field_kind, mapping.hash_is_binary
                ));
                // Таблица перечисления видов — отдельный объект метаданных, отдельный запрос.
                // Best-effort: если не удалось — enum_table пустой, watch-выгрузка уйдёт в .epf.
                let enum_table = fetch_enum_table(mcp).await.unwrap_or_else(|e| {
                    Logger::log(&format!(
                        "[{}] ⚠ таблица перечисления видов не определена: {} — файлы уйдут как .epf",
                        base.alias, e
                    ));
                    String::new()
                });
                StoredMapping {
                    table: mapping.table,
                    field_storage: mapping.field_storage,
                    field_hash: mapping.field_hash,
                    field_kind: mapping.field_kind,
                    enum_table,
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                }
            }
            ChangeDetection::Sql => {
                Logger::log(&format!(
                    "[{}] определение структуры хранения по служебным таблицам SQL...",
                    base.alias
                ));
                let client = sql_client
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("нет подключения к СУБД"))?;
                let m = crate::sql_discovery::discover_via_sql(client, meta_name).await?;
                Logger::log(&format!(
                    "[{}] mapping: table={} field_storage={} field_hash={} field_kind={} (binary_hash={})",
                    base.alias, m.table, m.field_storage, m.field_hash, m.field_kind, m.hash_is_binary
                ));
                StoredMapping {
                    table: m.table,
                    field_storage: m.field_storage,
                    field_hash: m.field_hash,
                    field_kind: m.field_kind,
                    enum_table: m.enum_table,
                    fetched_at: chrono::Utc::now().to_rfc3339(),
                }
            }
        };
        state.storage_mapping = Some(stored);
        state.save(&state_dir)?;
    }

    // 2. Обнаружение изменений: журнал регистрации либо отпечатки служебных таблиц.
    let trigger = match base.change_detection {
        ChangeDetection::Eventlog => {
            let mcp = mcp.ok_or_else(|| anyhow::anyhow!("режим eventlog без HTTP-клиента"))?;
            Trigger::Events(query_new_events(mcp, base, cfg, state).await?)
        }
        ChangeDetection::Sql => {
            let scope = SignalScope {
                base: base.export_base,
                extensions: base.export_extensions,
                processings: if base.export_processings {
                    state.storage_mapping.as_ref().map(StoredMappingLite::from_stored)
                } else {
                    None
                },
            };
            let client = sql_client
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("нет подключения к СУБД"))?;
            let signals = take_signals(client, &scope).await?;
            let reasons = diff_signals(state.sql_signals.as_ref(), &signals, &scope);
            let config_changed =
                crate::sql_signals::config_changed(state.sql_signals.as_ref(), &signals, &scope);
            Trigger::Signals { reasons, signals, config_changed }
        }
    };
    state.last_checked_at = Some(chrono::Utc::now().to_rfc3339());

    let count = match &trigger {
        Trigger::Events(events) => events.len(),
        Trigger::Signals { reasons, .. } => reasons.len(),
    };
    if count == 0 {
        state.save(&state_dir)?;
        Logger::log(&format!(
            "[{}] {}",
            base.alias,
            match &trigger {
                Trigger::Events(_) => "новых событий нет",
                Trigger::Signals { .. } => "изменений нет",
            }
        ));
        return Ok(0);
    }
    match &trigger {
        Trigger::Events(events) => {
            Logger::log(&format!(
                "[{}] найдено {} новых событий, запускаем выгрузку",
                base.alias, events.len()
            ));
            for e in events.iter().take(5) {
                Logger::log(&format!("    {} | {} | {}", e.date, e.event, e.user));
            }
            if events.len() > 5 {
                Logger::log(&format!("    ... и ещё {}", events.len() - 5));
            }
        }
        Trigger::Signals { reasons, .. } => {
            Logger::log(&format!(
                "[{}] обнаружены изменения ({}), запускаем выгрузку",
                base.alias, reasons.len()
            ));
            for r in reasons {
                Logger::log(&format!("    {}", r));
            }
        }
    }

    // 3. In-process выгрузка через ExportCoordinator (sync — выполняем в spawn_blocking)
    let mapping_for_proc: Option<ProcStorageMapping> = if base.export_processings {
        let m = state.storage_mapping.as_ref().expect("mapping должен быть после fetch выше");
        Some(ProcStorageMapping {
            table: m.table.clone(),
            field_storage: m.field_storage.clone(),
            field_hash: m.field_hash.clone(),
            field_kind: m.field_kind.clone(),
            enum_table: m.enum_table.clone(),
            // Бинарный rowversion: имя поля содержит "Version" (см. main.rs CLI override).
            hash_is_binary: m.field_hash.eq_ignore_ascii_case("_Version")
                || m.field_hash.to_lowercase().contains("version"),
        })
    } else {
        None
    };

    // Признак для снимка `base.cf`: в режиме sql он известен, в режиме журнала — нет.
    let config_changed = match &trigger {
        Trigger::Events(_) => None,
        Trigger::Signals { config_changed, .. } => Some(*config_changed),
    };

    let started = Instant::now();
    let base_clone = base.clone();
    let cfg_state_dir = state_dir.clone();
    let _ = cfg_state_dir; // силенсер если не используется
    let export_result = tokio::task::spawn_blocking(move || {
        run_export_for_base(&base_clone, mapping_for_proc, config_changed)
    })
    .await
    .map_err(|join_err| anyhow::anyhow!("spawn_blocking упал: {}", join_err))??;
    let _ = export_result;
    let duration = started.elapsed().as_secs();
    Logger::log(&format!("[{}] выгрузка завершена за {}с", base.alias, duration));

    // 4. git push
    let auth = match base.git_auth_type.as_str() {
        "password" => GitAuth::UserPassword {
            user: base.git_user.clone().unwrap_or_default(),
            password: base.git_password.clone().unwrap_or_default(),
        },
        _ => GitAuth::Domain,
    };
    let msg = format!(
        "auto: {} {}",
        base.alias,
        chrono::Local::now().format("%Y-%m-%d %H:%M")
    );
    let repo_path = PathBuf::from(&base.output_path);
    let push_repo = repo_path.clone();
    let remote_url = base.git_remote_url.clone();
    // Настройки git базы (core.autocrlf из bases.json).
    let git_opts = git_push::GitOptions::new(&base.git_autocrlf);
    let gc_opts = git_opts.clone();
    let push_result = tokio::task::spawn_blocking(move || {
        // origin берём из bases.json (gitRemoteUrl). Пусто — оставляем тот,
        // что уже прописан в каталоге выгрузки.
        git_push::ensure_repo_and_remote(&push_repo, &remote_url, &git_opts)?;
        // show_console=false: на сервере окно ушло бы в отключённый сеанс,
        // а причина отказа git должна попасть в лог и в журнал выгрузок.
        git_push::commit_and_push_with_console(&push_repo, &msg, &auth, false, &git_opts)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking git_push: {}", e))?
    .map_err(|e| anyhow::anyhow!("git_push: {}", e))?;
    let _ = push_result;

    // 4а. Phase 6: после успешного push безусловно удаляем остаточный
    //     external/processings/ — этот кэш не должен жить дольше успешного цикла.
    let cleanup_repo = repo_path.clone();
    let do_git_gc = base.git_gc_after_push;
    let gc_aggressive = base.git_gc_aggressive;
    tokio::task::spawn_blocking(move || {
        crate::export::force_remove_processings_cache(&cleanup_repo);
        // Phase 6.1: gc после чистки кэша. Best-effort — провал НЕ валит цикл,
        // gc это обслуживание, выгрузка к этому моменту уже зафиксирована push'ем.
        if do_git_gc {
            if let Err(e) = crate::git_push::git_gc(&cleanup_repo, gc_aggressive, &gc_opts) {
                crate::logging::Logger::log(&format!("⚠ git gc упал: {}", e));
            }
        }
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking cleanup: {}", e))?;

    // 5. update state: закладка журнала либо отпечатки, снятые до выгрузки.
    // Отпечатки именно доцикловые: если база изменилась во время выгрузки,
    // расхождение подхватит следующий цикл.
    match trigger {
        Trigger::Events(ref events) => mark_events_processed(state, events),
        Trigger::Signals { ref signals, .. } => state.sql_signals = Some(signals.clone()),
    }
    state.last_export_status = Some("ok".to_string());
    state.last_export_duration_sec = Some(duration);
    state.consecutive_failures = 0;
    state.save(&state_dir)?;

    // 6. flush proxy cache (best-effort, не валит цикл)
    if let Some(ref url) = cfg.cache_proxy_url {
        flush::send(url, &base.alias).await;
    }

    // Журнал выгрузок (state.db). Best-effort — провал записи не валит цикл.
    {
        // run_export_for_base возвращает () — детальный разбор по типам объектов
        // уже залогирован внутри export_full. Здесь фиксируем факт и число событий.
        let details: Vec<String> = match &trigger {
            Trigger::Events(events) => vec![format!("событий {}", events.len())],
            Trigger::Signals { reasons, .. } => reasons.clone(),
        };
        match crate::state_db::StateDb::open_default() {
            Ok(mut db) => {
                if let Err(e) = db.log_export(&crate::state_db::ExportLogEntry {
                    repo: base.alias.clone(),
                    finished_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                    duration_sec: Some(duration),
                    status: "ok".to_string(),
                    events: Some(count as u64),
                    details: Some(details.join(", ")),
                    error: None,
                }) {
                    Logger::log(&format!("[{}] ⚠ запись в export_log: {}", base.alias, e));
                }
            }
            Err(e) => Logger::log(&format!("[{}] ⚠ export_log open: {}", base.alias, e)),
        }
    }

    Ok(count)
}

/// Подключение к СУБД базы по её же настройкам выгрузки допобработок.
async fn connect_db(base: &BaseEntry) -> anyhow::Result<TiberiusClient> {
    let db_auth = if base.ibcmd_db_auth_windows {
        IbcmdDbAuth::Windows
    } else {
        IbcmdDbAuth::SqlLogin
    };
    let client = connect_mssql_raw(
        &base.sql_server,
        &base.sql_database,
        db_auth,
        base.db_user.as_deref(),
        base.db_pwd.as_deref(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("подключение к СУБД {}: {}", base.sql_server, e))?;
    Ok(client)
}

/// Sync-функция: загружает config, собирает IbcmdParams и вызывает ExportCoordinator.
/// Запускается в spawn_blocking, потому что внутри подпроцессы 1cv8.exe / ibcmd.exe.
fn run_export_for_base(
    base: &BaseEntry,
    processings_mapping: Option<ProcStorageMapping>,
    config_changed: Option<bool>,
) -> anyhow::Result<()> {
    let app_config = AppConfig::from_base(base);
    if let Err(errs) = app_config.validate() {
        anyhow::bail!("настройки базы '{}' невалидны: {:?}", base.alias, errs);
    }
    let ibcmd_path = app_config.ibcmd_path()?;

    let db_auth = if base.ibcmd_db_auth_windows {
        IbcmdDbAuth::Windows
    } else {
        IbcmdDbAuth::SqlLogin
    };

    // Авто-`--force` для первой выгрузки в пустую папку: если включён `--sync`,
    // но `<output_path>/base/ConfigDumpInfo.xml` отсутствует — ibcmd упал бы с
    // ошибкой синхронизации. Подмешиваем `--force`, чтобы он сделал полный дамп
    // (после первой успешной выгрузки ConfigDumpInfo.xml появится, и следующий
    // прогон пойдёт уже инкрементально без force).
    // Расширения и допобработки в аналогичной симметричной логике не нуждаются —
    // там пустота прошлой выгрузки уже обрабатывается естественно
    // (хеши расширений в state.db и load_manifest() возвращают пустые наборы).
    let auto_force = if base.ibcmd_sync {
        let dump_info = std::path::Path::new(&base.output_path)
            .join("base")
            .join("ConfigDumpInfo.xml");
        if !dump_info.exists() {
            Logger::log(&format!(
                "[{}] первая выгрузка в пустую папку: подмешиваем `--force` к ibcmd config export --sync",
                base.alias
            ));
            true
        } else {
            false
        }
    } else {
        false
    };

    let ibcmd_params = IbcmdParams {
        ibcmd_path,
        dbms: "MSSQLServer".to_string(),
        db_auth,
        db_user: base.db_user.clone(),
        db_pwd: base.db_pwd.clone(),
        use_connection_string: false,
        jobs: base.ibcmd_jobs.unwrap_or(0),
        sync: base.ibcmd_sync,
        force: auto_force,
        incremental_extensions: base.ibcmd_incremental,
    };

    let processings_params = if base.export_processings {
        Some(ProcessingsCliParams {
            sql_server: app_config.server.clone(),
            override_mapping: processings_mapping,
            rediscover: false,
            incremental: base.processings_incremental,
            // В watch-режиме источник не настраивается: сначала прямое определение
            // по MS SQL, при ошибке — HTTP-сервис базы, если он задан.
            discovery: crate::export::DiscoveryMode::Auto,
        })
    } else {
        None
    };

    let coordinator = ExportCoordinator::new(app_config).with_repo_id(&base.alias);
    let opts = ExportOptions {
        export_base: base.export_base,
        export_extensions: base.export_extensions,
        export_processings: base.export_processings,
        save_artifacts: base.save_artifacts,
        config_changed,
        ibcmd_params,
        processings_params,
    };

    let results = coordinator.export_full(&opts);

    if !results.overall_success() {
        anyhow::bail!("export_full вернул ошибки (см. лог выше)");
    }
    Ok(())
}

/// Конвертация StorageMapping (наша) → ProcStorageMapping (модуль processings).
/// Удобно, но в коде используем напрямую — этот From оставлен на случай нужды.
impl From<StorageMapping> for ProcStorageMapping {
    fn from(m: StorageMapping) -> Self {
        ProcStorageMapping {
            table: m.table,
            field_storage: m.field_storage,
            field_hash: m.field_hash,
            field_kind: m.field_kind,
            enum_table: m.enum_table,
            hash_is_binary: m.hash_is_binary,
        }
    }
}
