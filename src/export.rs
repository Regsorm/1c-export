use std::collections::HashMap;
use std::path::Path;
use chrono::Local;
use crate::config::AppConfig;
use crate::command_builder::{IbcmdBuilder, IbcmdParams};
use crate::processings::{self, ProcessingsResult, StorageMapping};
use crate::runner::ProcessRunner;
use crate::logging::Logger;

/// CLI-параметры выгрузки допобработок, которые приходят из main.rs
/// (не совпадают с `ProcessingsParams` из модуля processings — там ещё креды
/// из IBCMD-параметров, их `ExportCoordinator` добавит перед вызовом).
pub struct ProcessingsCliParams {
    /// Адрес MSSQL-сервера (обычно = 1С-сервер, override через --sql-server).
    pub sql_server: String,
    /// Override имён таблицы/полей (если заданы все четыре CLI-флага).
    pub override_mapping: Option<StorageMapping>,
    /// Форсировать повторный discovery через расширение.
    pub rediscover: bool,
    /// Инкрементальная выгрузка (true) или полная перезапись (false): при false
    /// модуль processings предварительно чистит External/ целиком.
    pub incremental: bool,
    /// Откуда брать SQL-имена таблицы и полей (см. `DiscoveryMode`).
    pub discovery: DiscoveryMode,
}

/// Источник SQL-имён таблицы и полей справочника допобработок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum DiscoveryMode {
    /// Сначала прямое определение по служебным таблицам MS SQL, при ошибке —
    /// HTTP-сервис MCP внутри базы (если он задан).
    #[default]
    Auto,
    /// Только прямое определение по служебным таблицам MS SQL.
    Sql,
    /// Только HTTP-сервис MCP внутри базы.
    Mcp,
}

/// Параметры полной выгрузки
pub struct ExportOptions {
    pub export_base: bool,
    pub export_extensions: bool,
    /// Режим выгрузки справочника ДополнительныеОтчетыИОбработки напрямую
    /// из MSSQL (без Designer). Независим от --export-base.
    pub export_processings: bool,
    /// Сохранять бинарные снимки `.cf`/`.cfe` в `_artifacts/` через `ibcmd config save`.
    /// По умолчанию выключено — каталог `_artifacts/` тогда не создаётся и не трогается.
    pub save_artifacts: bool,
    /// Менялась ли основная конфигурация в этом цикле.
    /// `None` — неизвестно (ручной запуск, режим eventlog) → снимок пишем как раньше.
    /// `Some(false)` — не менялась → снимок `_artifacts/base.cf` не переписываем,
    /// если он уже есть. Подробности — в `need_base_artifact`.
    pub config_changed: Option<bool>,
    pub ibcmd_params: IbcmdParams,
    /// Заполняется только если `export_processings = true`.
    pub processings_params: Option<ProcessingsCliParams>,
}

/// Результаты полной выгрузки
pub struct ExportResults {
    pub base: Option<bool>,
    pub extensions: Option<HashMap<String, bool>>,
    pub processings: Option<ProcessingsResult>,
}

impl ExportResults {
    /// Общий успех выгрузки. Единая семантика для GUI, CLI и watch:
    /// - основная конфигурация: false = провал;
    /// - расширения: хотя бы одно false (включая маркер `<list-failed>`) = провал;
    /// - допобработки: провал только при полном отказе (есть failed и ни одной
    ///   успешной записи) — частичные failed на отдельных записях не блокируют.
    pub fn overall_success(&self) -> bool {
        if let Some(false) = self.base {
            return false;
        }
        if let Some(ref ext) = self.extensions {
            if ext.values().any(|&v| !v) {
                return false;
            }
        }
        if let Some(ref pr) = self.processings {
            if !pr.failed.is_empty() && pr.new == 0 && pr.changed == 0 && pr.unchanged == 0 {
                return false;
            }
        }
        true
    }

    /// Краткая сводка для журнала выгрузок (колонка `details` в state.db).
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(ok) = self.base {
            parts.push(format!("база: {}", if ok { "✓" } else { "✗" }));
        }
        if let Some(ref ext) = self.extensions {
            if ext.contains_key("<list-failed>") {
                parts.push("расширения: список не получен".to_string());
            } else {
                let ok = ext.values().filter(|&&v| v).count();
                parts.push(format!("расширения: {}/{}", ok, ext.len()));
            }
        }
        if let Some(ref pr) = self.processings {
            parts.push(format!(
                "допобработки: new={} changed={} unchanged={} deleted={} failed={}",
                pr.new, pr.changed, pr.unchanged, pr.deleted, pr.failed.len()
            ));
        }
        parts.join("; ")
    }
}

/// Нужен ли новый бинарный снимок `_artifacts/base.cf` в этом цикле.
///
/// `ibcmd config save` на большой базе занимает минуты и даёт файл в гигабайты,
/// поэтому переписывать снимок, когда основная конфигурация не менялась, незачем.
/// - `save_artifacts = false` — снимки выключены вовсе.
/// - `full_rewrite = true` — база выгружена целиком (полный экспорт / `--force`),
///   снимок обновляем всегда.
/// - `config_changed = None` — неизвестно (ручной запуск, режим eventlog):
///   ведём себя как раньше и сохраняем.
/// - `config_changed = Some(false)` — конфигурация не менялась: сохраняем только
///   если снимка ещё нет (первый снимок нужен в любом случае).
pub fn need_base_artifact(
    save_artifacts: bool,
    config_changed: Option<bool>,
    full_rewrite: bool,
    cf_exists: bool,
) -> bool {
    if !save_artifacts {
        return false;
    }
    if full_rewrite {
        return true;
    }
    match config_changed {
        None | Some(true) => true,
        Some(false) => !cf_exists,
    }
}

/// Записать итог выгрузки в журнал `export_log` (state.db рядом с exe) —
/// его показывает вкладка «История» в GUI. Best-effort: провал записи журнала
/// логируется, но не влияет на результат выгрузки.
pub fn record_export_log(repo: &str, results: &ExportResults, duration_sec: u64) {
    let success = results.overall_success();
    let entry = crate::state_db::ExportLogEntry {
        repo: repo.to_string(),
        finished_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        duration_sec: Some(duration_sec),
        status: if success { "ok".to_string() } else { "fail".to_string() },
        events: None,
        details: Some(results.summary()),
        error: None,
    };
    match crate::state_db::StateDb::open_default() {
        Ok(mut db) => {
            if let Err(e) = db.log_export(&entry) {
                Logger::log(&format!("⚠ Запись в журнал выгрузок (state.db): {}", e));
            }
        }
        Err(e) => Logger::log(&format!("⚠ Журнал выгрузок: state.db не открылась: {}", e)),
    }
}

/// Координатор выгрузки
pub struct ExportCoordinator {
    config: AppConfig,
    /// Идентификатор репо в state.db (колонка `repo`). В watch = alias базы,
    /// в CLI/GUI — производное от имени папки output_path.
    repo_id: String,
}

impl ExportCoordinator {
    pub fn new(config: AppConfig) -> Self {
        let repo_id = Self::derive_repo_id(&config.output_path);
        Self { config, repo_id }
    }

    /// Переопределить repo_id (watch передаёт alias базы). Пустое — игнор.
    pub fn with_repo_id(mut self, repo_id: impl Into<String>) -> Self {
        let id = repo_id.into();
        if !id.is_empty() {
            self.repo_id = id;
        }
        self
    }

    /// repo-id по умолчанию: имя последней папки output_path (или "default").
    /// Запасной вариант, когда базы нет в bases.json и алиас взять неоткуда.
    pub fn derive_repo_id(output_path: &str) -> String {
        std::path::Path::new(output_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "default".to_string())
    }

    /// Выгрузка основной конфигурации.
    /// - `--sync` — инкрементальная синхронизация, папка НЕ очищается.
    /// - `--force` — полная перезапись, папка base/ очищается перед выгрузкой
    ///   (ibcmd отказывается писать в непустую папку, флаг --force документацией
    ///    привязан к --sync и реально не помогает для перезаписи папки).
    /// - `save_artifacts` — дополнительно писать бинарный снимок `_artifacts/base.cf`.
    /// - `config_changed` — менялась ли основная конфигурация (см. `need_base_artifact`).
    pub fn export_base(
        &self,
        params: &IbcmdParams,
        save_artifacts: bool,
        config_changed: Option<bool>,
    ) -> bool {
        Logger::separator();
        Logger::log("ВЫГРУЗКА ОСНОВНОЙ КОНФИГУРАЦИИ");

        let base_dir = Path::new(&self.config.output_path).join("base");

        // --sync требует наличия ConfigDumpInfo.xml в папке (результат предыдущего
        // полного экспорта). Если его нет — первый запуск: автоматически отключаем
        // --sync, делаем полный экспорт. Иначе ibcmd падает с ошибкой и пользователь
        // вынужден вручную переключать галку --force.
        let dump_info = base_dir.join("ConfigDumpInfo.xml");
        let mut effective = IbcmdParams {
            ibcmd_path: params.ibcmd_path.clone(),
            dbms: params.dbms.clone(),
            db_auth: params.db_auth,
            db_user: params.db_user.clone(),
            db_pwd: params.db_pwd.clone(),
            use_connection_string: params.use_connection_string,
            jobs: params.jobs,
            sync: params.sync,
            force: params.force,
            incremental_extensions: params.incremental_extensions,
        };
        if effective.sync && !dump_info.exists() {
            Logger::log(&format!(
                "ℹ --sync запрошен, но {} не найден — делаем полный экспорт (первый запуск).",
                dump_info.display()
            ));
            effective.sync = false;
        }
        // Если пользователь НЕ указал ни --sync, ни --force, но ConfigDumpInfo.xml уже есть —
        // умолчание: делаем инкремент (не перезапускать полный экспорт зря).
        if !effective.sync && !effective.force && dump_info.exists() {
            Logger::log(&format!(
                "ℹ Ни --sync, ни --force не указаны, но {} найден — автоматически включаем --sync.",
                dump_info.display()
            ));
            effective.sync = true;
        }

        if effective.sync {
            Logger::log("РЕЖИМ: Инкрементальная синхронизация (--sync)");
        } else if effective.force {
            Logger::log("РЕЖИМ: Полная перезапись (--force, папка base будет очищена)");
        } else {
            Logger::log("РЕЖИМ: Полный экспорт");
        }
        Logger::separator();

        // --force: очистить папку base/. Также — если нет --sync и папка есть с файлами:
        // ibcmd откажется писать в непустую папку, проще очистить самим.
        let need_clean = (effective.force || !effective.sync) && base_dir.exists();
        if need_clean {
            Logger::log(&format!("Очистка папки: {}", base_dir.display()));
            if let Err(e) = std::fs::remove_dir_all(&base_dir) {
                Logger::log(&format!("✗ Не удалось очистить папку: {}", e));
                return false;
            }
        }
        if let Err(e) = std::fs::create_dir_all(&base_dir) {
            Logger::log(&format!("✗ Не удалось создать папку: {}", e));
            return false;
        }

        let cmd = IbcmdBuilder::export_base(&effective, &self.config);
        let first_result = ProcessRunner::run(&cmd);

        // Авто-retry с `--force` при типовой ошибке `--sync`:
        //     "Требуется экспортировать конфигурацию полностью"
        // ibcmd выдаёт её, когда ConfigDumpInfo.xml в папке `base/` есть, но
        // несовместим (от Designer-выгрузки или старой версии ibcmd). Мы сразу
        // переключаемся на полную перезапись (с `--force`, чистим папку и
        // запускаем ту же команду без --sync). Это автоматический one-shot
        // recovery — следующий цикл watch уже пойдёт инкрементально.
        let needs_full_retry = match &first_result {
            Ok(r) if !r.success && effective.sync => {
                let body = format!("{}\n{}", r.stdout, r.stderr);
                body.contains("экспортировать конфигурацию полностью")
            }
            _ => false,
        };

        if needs_full_retry {
            Logger::log(
                "⚠ ibcmd: «Требуется экспортировать конфигурацию полностью» — \
                 ConfigDumpInfo.xml несовместим. Авто-retry с `--force` (полный дамп)."
            );
            let mut retry = effective.clone();
            retry.sync = false;
            retry.force = true;
            // Чистим папку base/ — после --force ibcmd хочет писать в пустую.
            if base_dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&base_dir) {
                    Logger::log(&format!("✗ Авто-retry: очистка {} упала: {}", base_dir.display(), e));
                    return false;
                }
            }
            if let Err(e) = std::fs::create_dir_all(&base_dir) {
                Logger::log(&format!("✗ Авто-retry: создание {} упало: {}", base_dir.display(), e));
                return false;
            }
            let retry_cmd = IbcmdBuilder::export_base(&retry, &self.config);
            return match ProcessRunner::run(&retry_cmd) {
                Ok(r2) if r2.success => {
                    Logger::log(&format!(
                        "✓ Основная конфигурация выгружена (после авто-retry с --force) в: {}",
                        base_dir.display()
                    ));
                    // Авто-retry идёт с `--force`, то есть базой перезаписанной
                    // целиком — снимок обновляем всегда.
                    self.maybe_save_base_artifact(&retry, save_artifacts, config_changed, true);
                    true
                }
                Ok(_) => {
                    Logger::log("✗ Авто-retry с `--force` тоже упал. Проверьте права на папку и stderr выше.");
                    false
                }
                Err(e) => {
                    Logger::log(&format!("✗ Авто-retry: ошибка запуска ibcmd: {}", e));
                    false
                }
            };
        }

        match first_result {
            Ok(result) => {
                if result.success {
                    Logger::log(&format!(
                        "✓ Основная конфигурация выгружена в: {}",
                        base_dir.display()
                    ));
                    // `!effective.sync` — база выгружена целиком (полный экспорт
                    // или `--force`), в этом случае снимок обновляем всегда.
                    self.maybe_save_base_artifact(
                        &effective,
                        save_artifacts,
                        config_changed,
                        !effective.sync,
                    );
                }
                result.success
            }
            Err(e) => {
                Logger::log(&format!("✗ Ошибка: {}", e));
                false
            }
        }
    }

    /// Обёртка над `save_base_artifact`: решает по `need_base_artifact`, нужен ли
    /// новый снимок. Пропуск — единственная строка в лог, чтобы в watch-режиме было
    /// видно, почему `base.cf` не переписан.
    fn maybe_save_base_artifact(
        &self,
        params: &IbcmdParams,
        save_artifacts: bool,
        config_changed: Option<bool>,
        full_rewrite: bool,
    ) {
        if !save_artifacts {
            return;
        }
        let cf_exists = Path::new(&self.config.output_path)
            .join("_artifacts")
            .join("base.cf")
            .exists();
        if need_base_artifact(save_artifacts, config_changed, full_rewrite, cf_exists) {
            self.save_base_artifact(params);
        } else {
            Logger::log("Снимок base.cf не обновляется: основная конфигурация не менялась");
        }
    }

    /// Бинарный snapshot конфигурации в `_artifacts/base.cf` через `ibcmd config save`.
    /// Опциональный шаг — вызывается после успешной XML-выгрузки. CF-файл нужен
    /// для надёжного деплоя на тех-стенд (`ibcmd config load + apply`), XML на стенде
    /// ненадёжна (карточки #4274, #3134, #2971). Кладётся в Git LFS через `.gitattributes`.
    /// Неуспех — логируется, но НЕ влияет на общий результат export_base (XML важнее).
    fn save_base_artifact(&self, params: &IbcmdParams) -> bool {
        let artifact_path = Path::new(&self.config.output_path)
            .join("_artifacts")
            .join("base.cf");
        let artifact_dir = artifact_path.parent().expect("у base.cf всегда есть parent");

        if let Err(e) = std::fs::create_dir_all(artifact_dir) {
            Logger::log(&format!("⚠ CF-артефакт: не удалось создать {}: {}", artifact_dir.display(), e));
            return false;
        }

        Logger::log(&format!("🔧 Бинарный snapshot конфигурации: {}", artifact_path.display()));
        let cmd = IbcmdBuilder::save_base(params, &self.config);
        match ProcessRunner::run(&cmd) {
            Ok(r) if r.success => {
                Logger::log(&format!("✓ CF-артефакт записан: {}", artifact_path.display()));
                true
            }
            Ok(r) => {
                let stderr = r.stderr.trim();
                Logger::log(&format!(
                    "⚠ CF-артефакт: ibcmd вернул код ошибки. stderr: {}",
                    if stderr.is_empty() { "<пусто>" } else { stderr }
                ));
                false
            }
            Err(e) => {
                Logger::log(&format!("⚠ CF-артефакт: ошибка запуска ibcmd: {}", e));
                false
            }
        }
    }

    /// Бинарный snapshot одного расширения в `_artifacts/extensions/<имя>.cfe`.
    /// Симметрично save_base_artifact — вызывается после успешной XML-выгрузки расширения,
    /// неуспех логируется, но не блокирует основной pipeline. Перезаписывает существующий
    /// .cfe (для изменившегося расширения новый бинарь должен заместить старый).
    fn save_extension_artifact(&self, params: &IbcmdParams, ext_name: &str) -> bool {
        let artifact_path = Path::new(&self.config.output_path)
            .join("_artifacts")
            .join("extensions")
            .join(format!("{}.cfe", ext_name));
        let artifact_dir = artifact_path.parent().expect("у <name>.cfe всегда есть parent");

        if let Err(e) = std::fs::create_dir_all(artifact_dir) {
            Logger::log(&format!("⚠ CFE-артефакт {}: не удалось создать {}: {}",
                ext_name, artifact_dir.display(), e));
            return false;
        }

        // ibcmd config save может отказаться писать поверх существующего файла
        // (поведение зависит от версии). Удаляем старый bin перед перезаписью.
        if artifact_path.exists() {
            if let Err(e) = std::fs::remove_file(&artifact_path) {
                Logger::log(&format!("⚠ CFE-артефакт {}: не удалось удалить старый {}: {}",
                    ext_name, artifact_path.display(), e));
                return false;
            }
        }

        Logger::log(&format!("🔧 Бинарный snapshot расширения {}: {}", ext_name, artifact_path.display()));
        let cmd = IbcmdBuilder::save_extension(params, &self.config, ext_name);
        match ProcessRunner::run(&cmd) {
            Ok(r) if r.success => {
                Logger::log(&format!("✓ CFE-артефакт записан: {}", artifact_path.display()));
                true
            }
            Ok(r) => {
                let stderr = r.stderr.trim();
                Logger::log(&format!(
                    "⚠ CFE-артефакт {}: ibcmd вернул код ошибки. stderr: {}",
                    ext_name,
                    if stderr.is_empty() { "<пусто>" } else { stderr }
                ));
                false
            }
            Err(e) => {
                Logger::log(&format!("⚠ CFE-артефакт {}: ошибка запуска ibcmd: {}", ext_name, e));
                false
            }
        }
    }

    /// Выгрузка расширений в двух режимах:
    /// - `incremental_extensions=true` — сравнивает hash-sum с прошлым запуском
    ///   (читает `git show HEAD:.extensions-hashes.json`), выгружает только изменившиеся,
    ///   удаляет папки расширений, которых больше нет в ИБ. Сохраняет новый `.extensions-hashes.json`.
    /// - `incremental_extensions=false` — полная перезапись (чистит всю папку `extensions/`).
    ///
    /// `save_artifacts` — дополнительно писать бинарные снимки `_artifacts/extensions/<имя>.cfe`.
    /// Если выключено, каталог `_artifacts/` не создаётся и не чистится вообще.
    pub fn export_extensions(&self, params: &IbcmdParams, save_artifacts: bool) -> HashMap<String, bool> {
        let repo = Path::new(&self.config.output_path).to_path_buf();
        let ext_dir = repo.join("extensions");
        // state.db (рядом с exe) — источник прошлых хешей расширений вместо
        // коммитимого .extensions-hashes.json. Не открылась — деградируем:
        // считаем что прошлых хешей нет (выгрузим всё), сохранение пропустим.
        let mut db = match crate::state_db::StateDb::open_default() {
            Ok(d) => Some(d),
            Err(e) => {
                Logger::log(&format!("⚠ state.db не открылась ({}): инкремент расширений отключён, выгружаю все", e));
                None
            }
        };

        Logger::separator();
        if params.incremental_extensions {
            Logger::log("ВЫГРУЗКА РАСШИРЕНИЙ (инкрементально по hash-sum)");
        } else {
            Logger::log("ВЫГРУЗКА ВСЕХ РАСШИРЕНИЙ (полная перезапись)");
        }
        Logger::separator();

        // Получаем свежий список (name, hash) из ИБ
        // Пустой список — тоже штатный случай: если раньше расширения были,
        // их папки и .cfe надо удалить, а хеши в state.db обнулить. Ранний выход
        // здесь оставлял на диске последнее удалённое расширение.
        let current = match self.list_extensions(params) {
            Some(list) => {
                if list.is_empty() {
                    Logger::log("Расширений в ИБ нет — выгружать нечего, проверяем удалённые");
                }
                list
            }
            None => {
                Logger::log("✗ Не удалось получить список расширений");
                let mut r = HashMap::new();
                r.insert("<list-failed>".to_string(), false);
                return r;
            }
        };
        let current_hashes: HashMap<String, String> = current.iter().cloned().collect();

        // Строим множества «к выгрузке» и «к удалению»
        let to_export: Vec<String>;
        let to_remove: Vec<String>;
        if params.incremental_extensions {
            let last_hashes = db
                .as_ref()
                .map(|d| d.load_extension_hashes(&self.repo_id).unwrap_or_default())
                .unwrap_or_default();
            if last_hashes.is_empty() {
                Logger::log("Прошлых хэшей нет (первый запуск, файл не закоммичен и нет на диске) — выгружаем ВСЕ расширения");
            } else {
                Logger::log(&format!(
                    "Прочитано прошлых хэшей: {} шт. (state.db, repo={})",
                    last_hashes.len(), self.repo_id
                ));
            }
            to_export = current.iter()
                .filter(|(name, hash)| {
                    // Если hash-sum отсутствует в выводе ibcmd — выгружаем принудительно
                    if hash.is_empty() { return true; }
                    last_hashes.get(name).map_or(true, |old| old != hash)
                })
                .map(|(n, _)| n.clone())
                .collect();
            to_remove = last_hashes.keys()
                .filter(|name| !current_hashes.contains_key(*name))
                .cloned()
                .collect();

            Logger::log(&format!(
                "К выгрузке: {} из {} (новые/изменённые). К удалению: {} (больше нет в ИБ).",
                to_export.len(), current.len(), to_remove.len()
            ));
            for n in &to_export { Logger::log(&format!("  + {}", n)); }
            for n in &to_remove { Logger::log(&format!("  − {}", n)); }
        } else {
            // Полная перезапись
            if ext_dir.exists() {
                Logger::log(&format!("Очистка папки: {}", ext_dir.display()));
                if let Err(e) = std::fs::remove_dir_all(&ext_dir) {
                    Logger::log(&format!("✗ Не удалось очистить папку: {}", e));
                    return HashMap::new();
                }
            }
            // Полная перезапись чистит и бинарные артефакты расширений (_artifacts/extensions/),
            // но только если снимки вообще сохраняются: иначе каталог чужой, его не трогаем.
            if save_artifacts {
                let cfe_art = repo.join("_artifacts").join("extensions");
                if cfe_art.exists() {
                    Logger::log(&format!("Полная перезапись: чистка {}", cfe_art.display()));
                    let _ = std::fs::remove_dir_all(&cfe_art);
                }
            }
            to_export = current.iter().map(|(n, _)| n.clone()).collect();
            to_remove = Vec::new();
        }

        if let Err(e) = std::fs::create_dir_all(&ext_dir) {
            Logger::log(&format!("✗ Не удалось создать папку: {}", e));
            return HashMap::new();
        }

        // Папка _artifacts/extensions/ для бинарных .cfe (Git LFS).
        let cfe_artifact_dir = repo.join("_artifacts").join("extensions");

        // Удаляем папки расширений, которых больше нет в ИБ. Заодно подчищаем .cfe-артефакты.
        for name in &to_remove {
            let dir = ext_dir.join(name);
            if dir.exists() {
                match std::fs::remove_dir_all(&dir) {
                    Ok(_) => Logger::log(&format!("✓ Удалена папка удалённого расширения: {}", name)),
                    Err(e) => Logger::log(&format!("✗ Не удалось удалить папку {}: {}", name, e)),
                }
            }
            if save_artifacts {
                let cfe = cfe_artifact_dir.join(format!("{}.cfe", name));
                if cfe.exists() {
                    match std::fs::remove_file(&cfe) {
                        Ok(_) => Logger::log(&format!("✓ Удалён CFE-артефакт удалённого расширения: {}", cfe.display())),
                        Err(e) => Logger::log(&format!("⚠ Не удалось удалить CFE-артефакт {}: {}", cfe.display(), e)),
                    }
                }
            }
        }

        // Выгружаем changed/new расширения
        let mut results = HashMap::new();
        let total = to_export.len();
        for (i, ext_name) in to_export.iter().enumerate() {
            Logger::log(&format!("\n[{}/{}] Выгрузка расширения: {}", i + 1, total, ext_name));

            let this_ext_dir = ext_dir.join(ext_name);
            // В инкрементальном режиме папка расширения могла остаться от прошлого запуска — чистим
            if this_ext_dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&this_ext_dir) {
                    Logger::log(&format!("✗ Не удалось очистить папку {}: {}", this_ext_dir.display(), e));
                    results.insert(ext_name.clone(), false);
                    continue;
                }
            }
            if let Err(e) = std::fs::create_dir_all(&this_ext_dir) {
                Logger::log(&format!("✗ Не удалось создать папку {}: {}", this_ext_dir.display(), e));
                results.insert(ext_name.clone(), false);
                continue;
            }

            let cmd = IbcmdBuilder::export_extension(params, &self.config, ext_name);
            let success = match ProcessRunner::run(&cmd) {
                Ok(result) => {
                    if result.success {
                        Logger::log(&format!("✓ Расширение {} выгружено", ext_name));
                    }
                    result.success
                }
                Err(e) => {
                    Logger::log(&format!("✗ Ошибка: {}", e));
                    false
                }
            };

            // После успешного XML-экспорта — параллельно бинарный CFE для тех-стенда.
            // Неуспех CFE не отменяет успех XML (CFE опционален, нужен только для деплоя).
            if success && save_artifacts {
                self.save_extension_artifact(params, ext_name);
            }
            results.insert(ext_name.clone(), success);
        }

        // Сохраняем кэш хэшей ВСЕГДА (не только в incremental-режиме). Это даёт
        // следующему запуску baseline для инкремента, даже если сейчас была полная
        // перезапись. Упавшие расширения остаются с ПРОШЛЫМ хэшем (или их нет в файле
        // вовсе, если первый запуск) — это намеренно: при следующем сравнении они
        // будут выглядеть «изменёнными» и перевыгрузятся.
        let all_ok = results.values().all(|&v| v);
        let hashes_to_save: HashMap<String, String> = if all_ok {
            // Все успешно — сохраняем все текущие хэши.
            current_hashes.clone()
        } else {
            // Есть упавшие — для них берём прошлый хэш из git (чтобы при следующем
            // запуске они снова попали в diff). Для успешных — текущий.
            let last = db
                .as_ref()
                .map(|d| d.load_extension_hashes(&self.repo_id).unwrap_or_default())
                .unwrap_or_default();
            current_hashes.iter()
                .map(|(name, cur)| {
                    let was_ok = results.get(name).copied().unwrap_or(false);
                    if was_ok {
                        (name.clone(), cur.clone())
                    } else {
                        // Если в прошлом запуске файла не было (last пустой) —
                        // оставляем «специальный» маркер пустой строки, чтобы
                        // в след. раз сравнение точно дало «не совпадает».
                        let prev = last.get(name).cloned().unwrap_or_default();
                        (name.clone(), prev)
                    }
                })
                .collect()
        };

        if let Some(d) = db.as_mut() {
            match d.save_extension_hashes(&self.repo_id, &hashes_to_save) {
                Ok(_) => {
                    Logger::log(&format!(
                        "✓ Сохранён кэш хэшей в state.db (repo={}, {} запис.)",
                        self.repo_id,
                        hashes_to_save.len()
                    ));
                    if !all_ok {
                        Logger::log(
                            "  ⚠ Часть расширений упала — для них сохранён прошлый хэш (либо пусто, если в БД их не было), чтобы при следующем запуске они снова попали в выгрузку."
                        );
                    }
                }
                Err(e) => Logger::log(&format!(
                    "⚠ Не удалось сохранить кэш хэшей в state.db (repo={}): {}",
                    self.repo_id, e
                )),
            }
        }
        let success_count = results.values().filter(|&&v| v).count();
        Logger::separator();
        if total == 0 {
            Logger::log("ИТОГИ: 0 расширений требовало обновления — всё актуально");
        } else {
            Logger::log(&format!("ИТОГИ: {}/{} расширений успешно", success_count, total));
        }
        results
    }

    /// Получить список расширений из ИБ: пары (name, hash-sum).
    /// Если в config.json задан ручной список — hash-sum в нём нет (инкрементальность не работает).
    fn list_extensions(&self, params: &IbcmdParams) -> Option<Vec<(String, String)>> {
        if !self.config.extensions.is_empty() {
            Logger::log(&format!(
                "Используется список расширений из config.json ({} шт., без hash-sum)",
                self.config.extensions.len()
            ));
            return Some(
                self.config.extensions.iter()
                    .map(|n| (n.clone(), String::new()))
                    .collect()
            );
        }

        Logger::log("Получение списка расширений через `ibcmd infobase config extension list`");
        let cmd = IbcmdBuilder::list_extensions(params, &self.config);
        let result = match ProcessRunner::run(&cmd) {
            Ok(r) if r.success => r,
            Ok(_) => return None,
            Err(e) => {
                Logger::log(&format!("✗ Ошибка: {}", e));
                return None;
            }
        };

        let extensions = Self::parse_extension_list(&result.stdout);
        if extensions.is_empty() && !result.stdout.trim().is_empty() {
            Logger::log("⚠ Парсер не распознал имена в выводе `extension list`. Сырой вывод:");
            for line in result.stdout.lines() {
                Logger::log(&format!("  | {}", line));
            }
        } else {
            Logger::log(&format!("✓ Найдено расширений: {}", extensions.len()));
            for (name, hash) in &extensions {
                let hash_preview = if hash.is_empty() {
                    "нет hash".to_string()
                } else {
                    format!("hash: {}…", &hash[..hash.len().min(12)])
                };
                Logger::log(&format!("  - {} [{}]", name, hash_preview));
            }
        }
        Some(extensions)
    }

    /// Парсинг вывода `ibcmd infobase config extension list`.
    ///
    /// Формат вывода (8.3.27) — блоки полей «key : value», разделённые пустой строкой.
    /// Для каждого расширения вытаскиваем пару (name, hash-sum):
    /// ```text
    /// name                         : "ДоработкаУТ"
    /// ...
    /// hash-sum                     : "GYr1lyk+HYnEdhf1V32HRRF0w30="
    /// ```
    /// hash-sum используется для инкрементальной выгрузки (сравнение с прошлым запуском).
    fn parse_extension_list(stdout: &str) -> Vec<(String, String)> {
        let mut result = Vec::new();
        let mut current_name: Option<String> = None;
        let mut current_hash: Option<String> = None;

        let flush = |result: &mut Vec<(String, String)>, name: &mut Option<String>, hash: &mut Option<String>| {
            if let Some(n) = name.take() {
                result.push((n, hash.take().unwrap_or_default()));
            } else {
                *hash = None;
            }
        };

        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                // Пустая строка — разделитель блоков. Фиксируем текущий блок.
                flush(&mut result, &mut current_name, &mut current_hash);
                continue;
            }

            let colon_pos = match trimmed.find(':') {
                Some(p) => p,
                None => continue,
            };
            let key = trimmed[..colon_pos].trim().to_lowercase();
            let value = trimmed[colon_pos + 1..].trim().trim_matches('"').trim().to_string();

            match key.as_str() {
                "name" | "имя" => {
                    // Начало нового блока — фиксируем предыдущий (если был)
                    flush(&mut result, &mut current_name, &mut current_hash);
                    if !value.is_empty() {
                        current_name = Some(value);
                    }
                }
                "hash-sum" | "hash" | "контрольная-сумма" => {
                    if !value.is_empty() {
                        current_hash = Some(value);
                    }
                }
                _ => {}
            }
        }
        // Финализируем последний блок
        flush(&mut result, &mut current_name, &mut current_hash);
        result
    }

    /// Полная выгрузка
    /// Одноразовая миграция старого состояния дельты из коммитимых файлов
    /// (`.extensions-hashes.json`, `External/_manifest.json`) в state.db.
    /// Срабатывает только если по этому repo в БД ещё нет данных, а файлы есть
    /// на диске — чтобы при первом запуске новой версии не делать лишнюю
    /// полную перевыгрузку и не падать на discovery допобработок без mcp_url.
    fn migrate_legacy_state(&self) {
        let mut db = match crate::state_db::StateDb::open_default() {
            Ok(d) => d,
            Err(e) => {
                Logger::log(&format!("предупреждение: миграция state.db пропущена (open: {})", e));
                return;
            }
        };
        let output = Path::new(&self.config.output_path);

        let ext_empty = db
            .load_extension_hashes(&self.repo_id)
            .map(|m| m.is_empty())
            .unwrap_or(true);
        if ext_empty {
            for rel in ["extensions/.extensions-hashes.json", ".extensions-hashes.json"] {
                let f = output.join(rel);
                if f.is_file() {
                    if let Ok(data) = std::fs::read(&f) {
                        if let Ok(map) = serde_json::from_slice::<HashMap<String, String>>(&data) {
                            if !map.is_empty() {
                                match db.save_extension_hashes(&self.repo_id, &map) {
                                    Ok(_) => Logger::log(&format!(
                                        "Миграция: {} хешей расширений из {} -> state.db (repo={})",
                                        map.len(), f.display(), self.repo_id
                                    )),
                                    Err(e) => Logger::log(&format!(
                                        "предупреждение: миграция хешей расширений не удалась: {}", e
                                    )),
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        let proc_empty = db
            .load_processings(&self.repo_id)
            .map(|m| m.is_empty())
            .unwrap_or(true);
        let disc_empty = db
            .load_discovery(&self.repo_id)
            .map(|d| d.is_none())
            .unwrap_or(true);
        if proc_empty || disc_empty {
            let ext_dir = output.join("External");
            if let Ok(manifest) = processings::load_manifest(&ext_dir) {
                if proc_empty && !manifest.items.is_empty() {
                    let items: std::collections::BTreeMap<String, crate::state_db::ProcItem> =
                        manifest
                            .items
                            .iter()
                            .map(|(u, m)| {
                                (
                                    u.clone(),
                                    crate::state_db::ProcItem {
                                        name: m.name.clone(),
                                        kind: m.kind.clone(),
                                        hash: m.hash.clone(),
                                        path: m.path.clone(),
                                        size: m.size,
                                        updated: m.updated.clone(),
                                    },
                                )
                            })
                            .collect();
                    let n = items.len();
                    if let Err(e) = db.save_processings(&self.repo_id, &items) {
                        Logger::log(&format!("предупреждение: миграция допобработок не удалась: {}", e));
                    } else {
                        Logger::log(&format!(
                            "Миграция: {} записей допобработок из _manifest.json -> state.db", n
                        ));
                    }
                }
                if disc_empty {
                    if let Some(src) = manifest.source {
                        let disc = crate::state_db::Discovery {
                            table: src.table,
                            field_storage: src.field_storage,
                            field_hash: src.field_hash,
                            field_kind: src.field_kind,
                            enum_table: src.enum_table,
                            hash_is_binary: src.hash_is_binary,
                            discovered_at: manifest.source_discovered_at,
                            kind_uuid_to_name: manifest.kind_uuid_to_name,
                        };
                        if let Err(e) = db.save_discovery(&self.repo_id, &disc) {
                            Logger::log(&format!("предупреждение: миграция discovery-кэша не удалась: {}", e));
                        } else {
                            Logger::log(
                                "Миграция: discovery-кэш допобработок из _manifest.json -> state.db"
                            );
                        }
                    }
                }
            }
        }
    }

    pub fn export_full(&self, opts: &ExportOptions) -> ExportResults {
        Logger::separator();
        Logger::log("НАЧАЛО ВЫГРУЗКИ");
        Logger::log(&format!("Время: {}", Local::now().format("%Y-%m-%d %H:%M:%S")));
        Logger::separator();

        let _ = std::fs::create_dir_all(&self.config.output_path);

        // Одноразовый импорт старого состояния дельты (если БД пуста по repo).
        self.migrate_legacy_state();

        let mut results = ExportResults {
            base: None,
            extensions: None,
            processings: None,
        };

        if opts.export_base {
            results.base = Some(self.export_base(
                &opts.ibcmd_params,
                opts.save_artifacts,
                opts.config_changed,
            ));
        }
        if opts.export_extensions {
            results.extensions = Some(self.export_extensions(&opts.ibcmd_params, opts.save_artifacts));
        }
        if opts.export_processings {
            results.processings = Some(self.export_processings(opts));
        }

        Logger::separator();
        Logger::log("ВЫГРУЗКА ЗАВЕРШЕНА");
        Logger::log(&format!("Время: {}", Local::now().format("%Y-%m-%d %H:%M:%S")));

        if let Some(s) = results.base {
            Logger::log(&format!("  Основная конфигурация: {}", if s { "✓ Успешно" } else { "✗ Ошибка" }));
        }
        if let Some(ref ext) = results.extensions {
            if ext.contains_key("<list-failed>") {
                Logger::log("  Расширения: ✗ не удалось получить список");
            } else if ext.is_empty() {
                // Инкрементально, список в ИБ есть, но ни одного не было выгружено
                // (все unchanged) — ИЛИ расширений в ИБ нет. Не зная — пишем нейтрально.
                Logger::log("  Расширения: 0 выгружено (всё актуально или список пуст)");
            } else {
                let ok = ext.values().filter(|&&v| v).count();
                let total = ext.len();
                Logger::log(&format!("  Расширения: {}/{} выгружено", ok, total));
            }
        }
        if let Some(ref proc_res) = results.processings {
            Logger::log(&format!(
                "  Доп. обработки: new={}, changed={}, unchanged={}, deleted={}, failed={}",
                proc_res.new,
                proc_res.changed,
                proc_res.unchanged,
                proc_res.deleted,
                proc_res.failed.len()
            ));
        }
        Logger::separator();
        results
    }

    /// Выгрузка справочника ДополнительныеОтчетыИОбработки через MSSQL.
    /// Авторизация SQL берётся из IBCMD-кредов (они и так заданы для основной выгрузки).
    pub fn export_processings(&self, opts: &ExportOptions) -> ProcessingsResult {
        Logger::separator();
        Logger::log("ВЫГРУЗКА ДОП. ОБРАБОТОК ИЗ MSSQL");
        Logger::separator();

        let cli_params = match &opts.processings_params {
            Some(p) => p,
            None => {
                Logger::log("✗ processings_params отсутствуют (внутренняя ошибка)");
                return ProcessingsResult {
                    failed: vec![("<params>".into(), "отсутствуют processings_params".into())],
                    ..Default::default()
                };
            }
        };

        // Базовая папка — <output>/External/. Там манифест, подпапка processings/ для .epf,
        // и подпапки <Имя>/ с XML-разбором. Всё коммитится в git.
        let output_dir = Path::new(&self.config.output_path).join("External");

        // Резолв StorageMapping: override из CLI > кэш в манифесте > fatal error
        // (автодискавери через расширение — следующий этап, пока не реализовано).
        let mapping = match self.resolve_processings_mapping(&output_dir, cli_params, &opts.ibcmd_params) {
            Ok(m) => m,
            Err(e) => {
                Logger::log(&format!("✗ Не удалось получить структуру хранения: {}", e));
                return ProcessingsResult {
                    failed: vec![("<mapping>".into(), e.to_string())],
                    ..Default::default()
                };
            }
        };

        // Карта UUID видов → представление больше не резолвится здесь: её строит
        // напрямую из SQL processings::run_async (build_kind_map, по mapping.enum_table).
        // Пустая карта на этом уровне не блокирует выгрузку — фолбэк на .epf.
        let params = processings::ProcessingsParams {
            sql_server: &cli_params.sql_server,
            repo_id: self.repo_id.clone(),
            incremental: cli_params.incremental,
            database: self.config.sql_database_name(),
            db_auth: opts.ibcmd_params.db_auth,
            db_user: opts.ibcmd_params.db_user.as_deref(),
            db_pwd: opts.ibcmd_params.db_pwd.as_deref(),
            mapping,
            kind_uuid_to_name: std::collections::HashMap::new(),
        };

        // Первый прогон. Если упало с "Invalid column/object name" —
        // вероятно, БСП обновилась и имена _FldNNN сдвинулись. Автоматически
        // перезапускаем discovery (с --rediscover=true) и пробуем ещё раз.
        let first = processings::export_processings(&params, &output_dir);
        let result_or_err = match first {
            Ok(r) => Ok(r),
            Err(e) => {
                let msg = e.to_string();
                let stale_mapping = msg.contains("Invalid column name")
                    || msg.contains("Invalid object name")
                    || msg.contains("Недопустимое имя столбца")
                    || msg.contains("Недопустимое имя объекта");
                if stale_mapping {
                    Logger::log(
                        "⚠ Imена таблицы/полей устарели (изменение конфигурации БСП?), \
                         автоматический rediscover и повторный запуск."
                    );
                    let forced_cli = ProcessingsCliParams {
                        sql_server: cli_params.sql_server.clone(),
                        override_mapping: None,
                        rediscover: true,
                        incremental: cli_params.incremental,
                        discovery: cli_params.discovery,
                    };
                    match self.resolve_processings_mapping(&output_dir, &forced_cli, &opts.ibcmd_params) {
                        Ok(new_mapping) => {
                            let retry_params = processings::ProcessingsParams {
                                sql_server: &cli_params.sql_server,
                                repo_id: self.repo_id.clone(),
                                incremental: cli_params.incremental,
                                database: self.config.sql_database_name(),
                                db_auth: opts.ibcmd_params.db_auth,
                                db_user: opts.ibcmd_params.db_user.as_deref(),
                                db_pwd: opts.ibcmd_params.db_pwd.as_deref(),
                                mapping: new_mapping,
                                kind_uuid_to_name: std::collections::HashMap::new(),
                            };
                            processings::export_processings(&retry_params, &output_dir)
                        }
                        Err(re) => Err(re),
                    }
                } else {
                    Err(e)
                }
            }
        };

        match result_or_err {
            Ok(mut result) => {
                Logger::log(&format!(
                    "✓ .epf: new={}, changed={}, unchanged={}, deleted={}, skipped={}, failed={}",
                    result.new, result.changed, result.unchanged, result.deleted,
                    result.skipped_empty.len(), result.failed.len()
                ));
                if !result.failed.is_empty() {
                    Logger::log("⚠ Записи с ошибками:");
                    for (k, v) in &result.failed {
                        Logger::log(&format!("   - {}: {}", k, v));
                    }
                }

                // Разбор свежих .epf/.erf в иерархию BSL+JSON — нативный разбор скелета
                // ExternalDataProcessor/ExternalReport, без внешних процессов, без
                // 1С-платформы и Designer'а. Неподдержанный вход — громкий провал в
                // result.failed, не молчаливый пропуск.
                if !result.fresh_names.is_empty() {
                    self.unpack_fresh_native(&output_dir, &result.fresh_names, &mut result.failed);
                }

                result
            }
            Err(e) => {
                Logger::log(&format!("✗ Ошибка выгрузки доп. обработок: {}", e));
                ProcessingsResult {
                    failed: vec![("<fatal>".into(), e.to_string())],
                    ..Default::default()
                }
            }
        }
    }

    /// Чистка корня External/ от побочных файлов Designer.
    /// Оставляем: _manifest.json и любые подпапки.
    /// Удаляем: всё остальное (файлы на верхнем уровне).
    fn cleanup_external_root(external_dir: &Path) {
        let entries = match std::fs::read_dir(external_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if name == "_manifest.json" {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&path) {
                Logger::log(&format!("⚠ не удалось удалить побочный файл {}: {}", path.display(), e));
            } else {
                Logger::log(&format!("  🗑 удалён побочный файл: {}", name));
            }
        }
    }

    /// Нативный разбор свежих .epf/.erf в иерархию BSL+JSON: каждый файл
    /// `<output>/processings/<name>.{epf|erf}` распаковывается в
    /// `<output>/processings_src/<name>/` (BSL-модули, Form.json,
    /// ExternalDataProcessor.json/ExternalReport.json и т.д.) через
    /// `v8container::unpack_epf_skeleton` — без внешних процессов, без
    /// 1С-платформы и Designer'а. Неподдержанный вход/ошибка разбора — громкая
    /// запись в `failed`, никакого молчаливого фолбэка/пропуска.
    fn unpack_fresh_native(
        &self,
        output_dir: &Path,
        fresh_names: &[String],
        failed: &mut Vec<(String, String)>,
    ) {
        Logger::separator();
        Logger::log(&format!(
            "native: разбор {} свежих .epf/.erf в иерархию BSL+JSON",
            fresh_names.len()
        ));
        Logger::separator();

        let processings_dir = output_dir.join("processings");
        let src_root = output_dir.join("processings_src");

        // Утилита: путь в виде с прямыми слэшами (для логов).
        fn p(path: &Path) -> String {
            path.display().to_string().replace('\\', "/")
        }

        // Параллельный разбор: общий индекс работы (AtomicUsize) + пул воркеров,
        // каждый забирает следующий свободный индекс через fetch_add. Потолок 4 —
        // унаследован от эпохи внешнего процесса v8unpack.exe (I/O-контенция на
        // диске); для нативного разбора (чтение .epf/.erf + запись BSL/JSON-дерева)
        // сохранён тот же лимит, отдельных замеров под native не проводилось.
        let total = fresh_names.len();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let failed_shared = std::sync::Mutex::new(Vec::<(String, String)>::new());
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(1, 4)
            .min(total.max(1));

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= total {
                            break;
                        }
                        let name = &fresh_names[i];

                        // Ищем файл по обоим расширениям — process_entry сохраняет .erf для отчётов
                        // и .epf для обработок (см. ext_by_kind). Берём то, что физически есть.
                        let candidate_epf = processings_dir.join(format!("{}.epf", name));
                        let candidate_erf = processings_dir.join(format!("{}.erf", name));
                        let (initial_path, initial_ext) = if candidate_epf.exists() {
                            (candidate_epf.clone(), ".epf")
                        } else if candidate_erf.exists() {
                            (candidate_erf.clone(), ".erf")
                        } else {
                            let msg = format!(
                                "исходник не найден: ни {} ни {} не существуют",
                                p(&candidate_epf), p(&candidate_erf)
                            );
                            Logger::log(&format!("⚠ [{}/{}] {}: {}", i + 1, total, name, msg));
                            failed_shared.lock().unwrap().push((name.clone(), msg));
                            continue;
                        };

                        let target = src_root.join(name);

                        // ЛОГИРУЕМ ДО запуска — чтобы при подвисании было ясно, какой файл крутится.
                        let size = std::fs::metadata(&initial_path).map(|m| m.len()).unwrap_or(0);
                        Logger::log(&format!(
                            "[{}/{}] разбор {}{} ({} байт) → {}",
                            i + 1,
                            total,
                            name,
                            initial_ext,
                            size,
                            p(&target)
                        ));

                        // Чистим целевую папку (если была — могут быть устаревшие файлы прошлых разборов).
                        if target.exists() {
                            if let Err(e) = std::fs::remove_dir_all(&target) {
                                let msg = format!("очистка {}: {}", p(&target), e);
                                Logger::log(&format!("  ⚠ {}", msg));
                                failed_shared.lock().unwrap().push((name.clone(), msg));
                                continue;
                            }
                        }
                        if let Err(e) = std::fs::create_dir_all(&target) {
                            let msg = format!("создание {}: {}", p(&target), e);
                            Logger::log(&format!("  ⚠ {}", msg));
                            failed_shared.lock().unwrap().push((name.clone(), msg));
                            continue;
                        }

                        // Нативная распаковка скелета ExternalDataProcessor/ExternalReport —
                        // единственный путь разбора, без фолбэка на внешний процесс. Done —
                        // файлы уже записаны в target, бинарный исходник больше не нужен.
                        // Unsupported/ошибка разбора/ошибка чтения — native ничего не пишет
                        // (см. докстринг unpack_epf_skeleton), target остаётся пустой — чистим
                        // её и громко фиксируем провал в failed, без молчаливого пропуска.
                        let epf_bytes = match std::fs::read(&initial_path) {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                let msg = format!("не прочитать {}{}: {}", name, initial_ext, e);
                                Logger::log(&format!("  ⚠ {}", msg));
                                let _ = std::fs::remove_dir_all(&target);
                                failed_shared.lock().unwrap().push((name.clone(), msg));
                                continue;
                            }
                        };
                        match crate::v8container::unpack_epf_skeleton(&epf_bytes, &target) {
                            Ok(crate::v8container::UnpackOutcome::Done) => {
                                Logger::log(&format!(
                                    "  ✓ [{}/{}] {}: нативный разбор",
                                    i + 1,
                                    total,
                                    name
                                ));
                                if let Err(e) = std::fs::remove_file(&initial_path) {
                                    Logger::log(&format!("  ⚠ не удалось удалить {}: {}", p(&initial_path), e));
                                }
                                continue;
                            }
                            Ok(crate::v8container::UnpackOutcome::Unsupported(reason)) => {
                                let msg = format!("не поддерживается: {}", reason);
                                Logger::log(&format!("  ⚠ {}", msg));
                                let _ = std::fs::remove_dir_all(&target);
                                failed_shared.lock().unwrap().push((name.clone(), msg));
                                continue;
                            }
                            Err(e) => {
                                let msg = format!("ошибка разбора: {}", e);
                                Logger::log(&format!("  ⚠ {}", msg));
                                let _ = std::fs::remove_dir_all(&target);
                                failed_shared.lock().unwrap().push((name.clone(), msg));
                                continue;
                            }
                        }
                    }
                });
            }
        });

        // Собираем провалы из воркеров, сортируем по имени для стабильного вывода.
        let mut fv = failed_shared.into_inner().unwrap();
        fv.sort_by(|a, b| a.0.cmp(&b.0));
        failed.extend(fv);

        // ─── Финализация структуры output_dir ─────────────────────────────────
        // Цель: в корне output_dir (External/) должны остаться только папки
        // обработок (распакованные) и .erf-бинари отчётов + _manifest.json.
        // Служебные подкаталоги processings/, processings_src/, v8unpack_temp/
        // удаляются. Эта структура «как было в Python-версии».
        Logger::separator();
        Logger::log("Финализация структуры External/: переносим в корень и чистим служебные папки");
        Logger::separator();

        // 1) Перенос распакованных обработок: processings_src/<name>/ → <output_dir>/<name>/
        let mut moved_dirs = 0usize;
        let mut moved_files = 0usize;
        let mut path_renames: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        if src_root.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&src_root) {
                for entry in entries.flatten() {
                    let from = entry.path();
                    let Some(fname) = from.file_name() else { continue };
                    let to = output_dir.join(fname);
                    if to.exists() {
                        let _ = std::fs::remove_dir_all(&to);
                    }
                    match std::fs::rename(&from, &to) {
                        Ok(_) => {
                            moved_dirs += 1;
                            let key = format!("processings/{}.epf", fname.to_string_lossy());
                            let key2 = format!("processings/{}.erf", fname.to_string_lossy());
                            let new_path = fname.to_string_lossy().to_string();
                            path_renames.insert(key, new_path.clone());
                            path_renames.insert(key2, new_path);
                        }
                        Err(e) => Logger::log(&format!(
                            "⚠ не удалось перенести {} → {}: {}",
                            p(&from), p(&to), e
                        )),
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&src_root);
        }

        // 2) Перенос оставшихся бинарей: processings/<name>.erf|.epf → <output_dir>/<name>.erf|.epf
        //    (файлы, для которых нативный разбор не сработал — Unsupported/ошибка,
        //    причина уже зафиксирована в failed; бинарь не теряем).
        if processings_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&processings_dir) {
                for entry in entries.flatten() {
                    let from = entry.path();
                    if !from.is_file() { continue; }
                    let Some(fname) = from.file_name() else { continue };
                    let to = output_dir.join(fname);
                    if to.exists() {
                        let _ = std::fs::remove_file(&to);
                    }
                    match std::fs::rename(&from, &to) {
                        Ok(_) => {
                            moved_files += 1;
                            let old_key = format!("processings/{}", fname.to_string_lossy());
                            path_renames.insert(old_key, fname.to_string_lossy().to_string());
                        }
                        Err(e) => Logger::log(&format!(
                            "⚠ не удалось перенести {} → {}: {}",
                            p(&from), p(&to), e
                        )),
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&processings_dir);
        }

        // 3) Удаление v8unpack_temp/ — legacy-чистка каталога от эпохи внешнего процесса
        //    v8unpack.exe (native его больше не создаёт). Оставлено на случай, если
        //    каталог остался от прошлого запуска до перехода на native-only разбор.
        let v8temp_root = output_dir.join("v8unpack_temp");
        if v8temp_root.is_dir() {
            let _ = std::fs::remove_dir_all(&v8temp_root);
        }

        // 4) Обновляем path в _manifest.json — чтобы при следующем запуске
        //    логика «удалить осиротевших» искала файлы по корректным путям.
        if !path_renames.is_empty() {
            if let Ok(mut db) = crate::state_db::StateDb::open_default() {
                if let Ok(mut items) = db.load_processings(&self.repo_id) {
                    let mut updated = 0usize;
                    for item in items.values_mut() {
                        if let Some(new_path) = path_renames.get(&item.path) {
                            item.path = new_path.clone();
                            updated += 1;
                        }
                    }
                    if updated > 0 {
                        if let Err(e) = db.save_processings(&self.repo_id, &items) {
                            Logger::log(&format!("⚠ не удалось обновить пути в state.db: {}", e));
                        } else {
                            Logger::log(&format!("✓ state.db: обновлено путей {}", updated));
                        }
                    }
                }
            }
        }

        Logger::log(&format!(
            "✓ Финализация: перенесено в корень {} папок (обработки) + {} файлов (отчёты-бинари)",
            moved_dirs, moved_files
        ));
    }

    /// Определить имена таблицы/полей для справочника ДополнительныеОтчетыИОбработки.
    /// Приоритет: CLI override > кэш в _manifest.json > (в будущем) автодискавери через расширение.
    fn resolve_processings_mapping(
        &self,
        output_dir: &Path,
        cli: &ProcessingsCliParams,
        ibcmd: &IbcmdParams,
    ) -> Result<StorageMapping, crate::error::ExportError> {
        // 1) Override из CLI имеет наивысший приоритет.
        if let Some(ref m) = cli.override_mapping {
            Logger::log(&format!(
                "✓ Используем mapping из CLI: table={}, storage={}, hash={}, kind={}",
                m.table, m.field_storage, m.field_hash, m.field_kind
            ));
            return Ok(m.clone());
        }

        // 2) Кэш в манифесте (если не --rediscover).
        //    Валидируем: имена должны начинаться с "_" (физические имена MSSQL).
        //    Старый/битый кэш (без префикса или без hash-колонки) — игнорируем,
        //    перезапускаем discovery автоматически.
        if !cli.rediscover {
            let db_ro = crate::state_db::StateDb::open_default().ok();
            let cached = db_ro
                .as_ref()
                .and_then(|d| d.load_discovery(&self.repo_id).ok().flatten());
            if let Some(disc) = cached {
                let m = StorageMapping {
                    table: disc.table,
                    field_storage: disc.field_storage,
                    field_hash: disc.field_hash,
                    field_kind: disc.field_kind,
                    enum_table: disc.enum_table,
                    hash_is_binary: disc.hash_is_binary,
                };
                let valid = m.table.starts_with('_')
                    && m.field_storage.starts_with('_')
                    && m.field_hash.starts_with('_');
                if valid {
                    Logger::log(&format!(
                        "✓ Используем mapping из кэша _manifest.json: table={}",
                        m.table
                    ));
                    return Ok(m);
                } else {
                    Logger::log(&format!(
                        "⚠ Кэш _manifest.json невалидный (имена без префикса '_' или битые), \
                         перезапускаем discovery. Было: table={}, storage={}, hash={}",
                        m.table, m.field_storage, m.field_hash
                    ));
                }
            }
        }

        // 3) Определение структуры. По умолчанию (auto) — напрямую по служебным
        //    таблицам MS SQL (Params/Config): ни 1С, ни HTTP-сервис в базе не нужны.
        //    HTTP-сервис MCP остаётся запасным путём — на случай, когда прямого
        //    доступа к СУБД нет или её служебные таблицы разобрать не удалось.
        let mcp_available =
            !self.config.mcp_url.trim().is_empty() && !self.config.mcp_api_key.trim().is_empty();

        let mapping = match cli.discovery {
            DiscoveryMode::Mcp => {
                if !mcp_available {
                    return Err(crate::error::ExportError::CommandFailed {
                        code: -1,
                        message: format!(
                            "Запрошен --discovery=mcp, но HTTP-сервис не задан \
                             (mcpUrl='{}', mcpApiKey={}).",
                            self.config.mcp_url,
                            if self.config.mcp_api_key.is_empty() { "<пусто>" } else { "<задан>" }
                        ),
                    });
                }
                Logger::log(&format!(
                    "Определение структуры хранения через MCP HTTP: {}",
                    self.config.mcp_url
                ));
                self.discover_via_mcp()?
            }
            DiscoveryMode::Sql => {
                Logger::log("Определение структуры хранения напрямую по служебным таблицам MS SQL");
                self.discover_via_sql(cli, ibcmd)?
            }
            DiscoveryMode::Auto => {
                Logger::log("Определение структуры хранения напрямую по служебным таблицам MS SQL");
                match self.discover_via_sql(cli, ibcmd) {
                    Ok(m) => m,
                    Err(sql_err) => {
                        if !mcp_available {
                            return Err(crate::error::ExportError::CommandFailed {
                                code: -1,
                                message: format!(
                                    "Не удалось определить структуру хранения справочника допобработок.\n\
                                     Прямое определение по MS SQL не сработало: {}\n\
                                     Запасной путь (HTTP-сервис MCP в ИБ) недоступен: \
                                     mcpUrl='{}', mcpApiKey={}.\n\
                                     Варианты: дать доступ к СУБД, задать mcpUrl/mcpApiKey, \
                                     передать известный mapping через CLI override \
                                     или выключить выгрузку допобработок.",
                                    sql_err,
                                    self.config.mcp_url,
                                    if self.config.mcp_api_key.is_empty() { "<пусто>" } else { "<задан>" }
                                ),
                            });
                        }
                        Logger::log(&format!(
                            "⚠ Прямое определение по MS SQL не удалось: {} — пробуем HTTP-сервис MCP: {}",
                            sql_err, self.config.mcp_url
                        ));
                        self.discover_via_mcp()?
                    }
                }
            }
        };

        Logger::log(&format!(
            "✓ Структура хранения определена: table={} field_storage={} field_hash={} (binary={}) enum_table={}",
            mapping.table, mapping.field_storage, mapping.field_hash, mapping.hash_is_binary,
            if mapping.enum_table.is_empty() { "<нет>" } else { &mapping.enum_table }
        ));

        // Кэшируем в state.db (proc_discovery), мержа с уже сохранённой kind-частью.
        if let Ok(mut db) = crate::state_db::StateDb::open_default() {
            let mut disc = db.load_discovery(&self.repo_id).ok().flatten().unwrap_or(
                crate::state_db::Discovery {
                    table: String::new(),
                    field_storage: String::new(),
                    field_hash: String::new(),
                    field_kind: String::new(),
                    enum_table: String::new(),
                    hash_is_binary: false,
                    discovered_at: None,
                    kind_uuid_to_name: std::collections::HashMap::new(),
                },
            );
            disc.table = mapping.table.clone();
            disc.field_storage = mapping.field_storage.clone();
            disc.field_hash = mapping.field_hash.clone();
            disc.field_kind = mapping.field_kind.clone();
            disc.enum_table = mapping.enum_table.clone();
            disc.hash_is_binary = mapping.hash_is_binary;
            disc.discovered_at =
                Some(chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string());
            if let Err(e) = db.save_discovery(&self.repo_id, &disc) {
                Logger::log(&format!("⚠ не удалось сохранить discovery в state.db: {}", e));
            }
        }

        Ok(mapping)
    }

    /// Спросить структуру хранения справочника допобработок через HTTP-сервис MCP в ИБ.
    /// Это тот же путь, что watch-режим использует через `fetch_storage_mapping` —
    /// никакого `1cv8.exe` локально не запускается.
    ///
    /// Внутри запускает однопоточный tokio runtime и конвертирует результат
    /// `storage_mapping::StorageMapping` → `processings::StorageMapping`.
    /// Определить структуру хранения напрямую по служебным таблицам MS SQL
    /// (`Params.DBNames` + `Config`) — без 1С и без HTTP-сервиса в базе.
    /// Учётные данные для СУБД те же, что у выгрузки допобработок (IBCMD-креды).
    fn discover_via_sql(
        &self,
        cli: &ProcessingsCliParams,
        ibcmd: &IbcmdParams,
    ) -> Result<processings::StorageMapping, crate::error::ExportError> {
        let meta_name = if self.config.processings_meta_name.trim().is_empty() {
            "Справочник.ДополнительныеОтчетыИОбработки"
        } else {
            self.config.processings_meta_name.as_str()
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| crate::error::ExportError::CommandFailed {
                code: -1,
                message: format!("tokio runtime: {}", e),
            })?;

        rt.block_on(async {
            let mut client = processings::connect_mssql_raw(
                &cli.sql_server,
                self.config.sql_database_name(),
                ibcmd.db_auth,
                ibcmd.db_user.as_deref(),
                ibcmd.db_pwd.as_deref(),
            )
            .await?;
            crate::sql_discovery::discover_via_sql(&mut client, meta_name)
                .await
                .map_err(|e| crate::error::ExportError::Sql(format!("{:#}", e)))
        })
    }

    fn discover_via_mcp(&self) -> Result<processings::StorageMapping, crate::error::ExportError> {
        use crate::mcp_client::McpClient;
        use crate::storage_mapping::{fetch_enum_table, fetch_storage_mapping};

        let meta_name = if self.config.processings_meta_name.trim().is_empty() {
            "Справочник.ДополнительныеОтчетыИОбработки"
        } else {
            self.config.processings_meta_name.as_str()
        };

        let client = McpClient::new(
            &self.config.mcp_url,
            &self.config.authentication.login,
            &self.config.authentication.password,
            self.config.mcp_api_key.clone(),
        )
        .map_err(|e| crate::error::ExportError::CommandFailed {
            code: -1,
            message: format!("McpClient init: {}", e),
        })?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| crate::error::ExportError::CommandFailed {
                code: -1,
                message: format!("tokio runtime: {}", e),
            })?;

        let mapping_http = rt
            .block_on(fetch_storage_mapping(&client, meta_name))
            .map_err(|e| crate::error::ExportError::CommandFailed {
                code: -1,
                message: format!("fetch_storage_mapping: {}", e),
            })?;

        // Таблица перечисления видов — отдельный объект метаданных, резолвится
        // отдельным запросом. Не блокирующая операция: если не удалось — карта
        // видов останется пустой, process_entry сохранит файлы как .epf.
        let enum_table = rt.block_on(fetch_enum_table(&client)).unwrap_or_else(|e| {
            Logger::log(&format!(
                "⚠ не удалось определить таблицу перечисления видов: {} — все файлы пойдут как .epf",
                e
            ));
            String::new()
        });

        Ok(processings::StorageMapping {
            table: mapping_http.table,
            field_storage: mapping_http.field_storage,
            field_hash: mapping_http.field_hash,
            field_kind: mapping_http.field_kind,
            enum_table,
            hash_is_binary: mapping_http.hash_is_binary,
        })
    }
}

/// Phase 6: безусловно удалить служебный каталог `external/processings/`
/// (временный кэш сырых .epf/.erf бинарей, который должен исчезать после
/// каждой успешной выгрузки допобработок).
///
/// В нормальном пайплайне он удаляется в финализации `process_processings_export`,
/// но если что-то упало в середине (нечитаемый файл / прерывание) — каталог
/// может остаться. Безусловный pass в конце цикла гарантирует, что в
/// репозитории не появится случайных `.epf`/`.erf` бинарей в `external/processings/`.
pub fn force_remove_processings_cache(output_dir: &Path) {
    let processings = output_dir.join("external").join("processings");
    if processings.is_dir() {
        match std::fs::remove_dir_all(&processings) {
            Ok(_) => Logger::log(&format!(
                "✓ Удалён остаточный кэш {}",
                processings.display()
            )),
            Err(e) => Logger::log(&format!(
                "⚠ не удалось удалить {}: {}",
                processings.display(), e
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::need_base_artifact;

    #[test]
    fn artifacts_off_never_saves() {
        for config_changed in [None, Some(true), Some(false)] {
            for full_rewrite in [false, true] {
                for cf_exists in [false, true] {
                    assert!(!need_base_artifact(false, config_changed, full_rewrite, cf_exists));
                }
            }
        }
    }

    #[test]
    fn unknown_config_state_saves_as_before() {
        assert!(need_base_artifact(true, None, false, true));
        assert!(need_base_artifact(true, None, false, false));
    }

    #[test]
    fn changed_config_saves() {
        assert!(need_base_artifact(true, Some(true), false, true));
        assert!(need_base_artifact(true, Some(true), false, false));
    }

    #[test]
    fn unchanged_config_skips_only_when_snapshot_exists() {
        // Снимок есть — второй раз те же гигабайты не пишем.
        assert!(!need_base_artifact(true, Some(false), false, true));
        // Снимка нет — первый нужен в любом случае.
        assert!(need_base_artifact(true, Some(false), false, false));
    }

    #[test]
    fn full_rewrite_always_saves() {
        assert!(need_base_artifact(true, Some(false), true, true));
        assert!(need_base_artifact(true, None, true, true));
    }
}
