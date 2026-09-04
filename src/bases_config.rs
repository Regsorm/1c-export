//! Парсер `bases.json` — глобального реестра баз для watch-mode.
//!
//! Формат: один JSON-файл с глобальными настройками (интервал, MCP-URL,
//! whitelist триггер-событий) и массивом `bases[]`. Каждая запись описывает
//! одну базу: алиас, путь к её отдельному `configs/<alias>.json`, целевой
//! git-репо и набор флагов выгрузки.
//!
//! Файл лежит вне исходников 1c-export (например, `C:/Projects/1c-export-daemon/bases.json`)
//! и в репо не коммитится — содержит специфику деплоя.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Корневая структура `bases.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    /// Интервал между циклами watch-loop (минуты). Дефолт 30.
    #[serde(default = "default_check_interval_minutes")]
    pub check_interval_minutes: u64,

    /// Окно для первого запуска (если state-файл базы пуст). Дефолт 168 (неделя).
    #[serde(default = "default_lookback_first_run")]
    pub lookback_hours_first_run: u64,


    /// URL прокси-кэша (когда появится). Если null — flush не отправляется.
    #[serde(default)]
    pub cache_proxy_url: Option<String>,

    /// Через сколько дней принудительно перевыполнить fetch_storage_mapping.
    /// Дефолт 30 — структуру SQL-таблицы перетряхивают редко.
    #[serde(default = "default_refetch_storage")]
    pub refetch_storage_mapping_after_days: u64,

    /// Каталог для state-файлов. Дефолт "./state".
    #[serde(default = "default_state_dir")]
    pub state_dir: String,

    /// Каталог для логов. Дефолт "./logs".
    #[serde(default = "default_log_dir")]
    pub log_dir: String,

    /// Подробность журнала: "info" (дефолт) или "debug". Неизвестное значение
    /// трактуется как "info" — отдельной валидации нет.
    #[serde(default = "default_log_level", rename = "logLevel", alias = "log_level")]
    pub log_level: String,

    /// Whitelist системных событий 1С — триггеров выгрузки.
    /// По умолчанию: применение конфигурации к БД и три CRUD-события расширений.
    #[serde(default = "default_trigger_events")]
    pub trigger_events: Vec<String>,

    /// Полное имя справочника допобработок в метаданных 1С (БСП-функционал, имя
    /// одно для всех баз с БСП). По умолчанию "Справочник.ДополнительныеОтчетыИОбработки".
    /// На уровне `BaseEntry` есть опциональный per-base override.
    #[serde(default, rename = "processingsMetaName", alias = "processings_meta_name")]
    pub processings_meta_name: String,

    /// Список баз под наблюдением.
    pub bases: Vec<BaseEntry>,
}

/// Как watch узнаёт об изменениях базы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeDetection {
    /// Опрос служебных таблиц MS SQL (Config, _ExtensionsInfo, таблица справочника
    /// допобработок). HTTP-сервис в базе не нужен.
    #[default]
    Sql,
    /// Журнал регистрации через HTTP-сервис MCP внутри базы (`mcpUrl`, `mcpApiKey`).
    Eventlog,
}

/// Одна запись в `bases[]` — самодостаточный набор настроек одной базы.
/// Все настройки базы (SQL, MCP, 1С, git, флаги выгрузки) в одном объекте,
/// без отдельных файлов `configs/<alias>.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BaseEntry {
    /// Уникальный алиас базы (используется в логах, имени state-файла, идентификации).
    pub alias: String,

    // ── SQL Server (MSSQL) ──────────────────────────────────────────────────
    /// Адрес MSSQL-сервера. Используется для прямого TDS-коннекта при выгрузке
    /// допобработок и для IBCMD `--db-server=`.
    #[serde(rename = "sqlServer", alias = "sql_server")]
    pub sql_server: String,

    /// Имя физической БД на MSSQL — для TDS-коннекта и IBCMD `--db-name=`.
    #[serde(rename = "sqlDatabase", alias = "sql_database")]
    pub sql_database: String,

    /// Адрес сервера приложений 1С (кластер) — для ENTERPRISE/DESIGNER-операций
    /// (`/S server\db`): legacy CLI-дамп .epf и ENTERPRISE-автодискавери.
    /// Может отличаться от `sqlServer` (СУБД) и от хоста `mcpUrl` (сервис ИБ).
    /// Если пусто — fallback на `sqlServer` (см. AppConfig::server_for_1c).
    #[serde(default, rename = "server1C", alias = "server_1c")]
    pub server_1c: String,

    /// Использовать Windows-аутентификацию для коннектов к MSSQL. Дефолт true.
    /// Если false — нужны `dbUser` / `dbPwd`.
    #[serde(default = "default_true", rename = "ibcmdDbAuthWindows", alias = "ibcmd_db_auth_windows")]
    pub ibcmd_db_auth_windows: bool,

    /// Логин SQL Server (для SQL-аутентификации). Уходит и в IBCMD (`--db-user`),
    /// и в прямой TDS-коннект для processings.
    #[serde(default, rename = "dbUser", alias = "db_user")]
    pub db_user: Option<String>,

    /// Пароль SQL Server (для SQL-аутентификации).
    #[serde(default, rename = "dbPwd", alias = "db_pwd")]
    pub db_pwd: Option<String>,

    // ── Обнаружение изменений ───────────────────────────────────────────────
    /// Способ обнаружения изменений базы в watch: `sql` (по умолчанию) или `eventlog`.
    #[serde(default, rename = "changeDetection", alias = "change_detection")]
    pub change_detection: ChangeDetection,

    // ── 1С: HTTP-сервис MCP внутри ИБ ───────────────────────────────────────
    /// Полный URL HTTP-сервиса MCP в ИБ — например, `http://1c-server/demo-ut/hs/mcp`.
    /// Watch шлёт сюда `tools/call`-запросы (`eventlog_query`, `db_table_fields`).
    /// Нужен только при `changeDetection = eventlog`.
    #[serde(default, rename = "mcpUrl", alias = "mcp_url")]
    pub mcp_url: String,

    /// API-ключ HTTP-сервиса MCP в 1С (заголовок `X-MCP-Key`).
    /// Нужен только при `changeDetection = eventlog`.
    #[serde(default, rename = "mcpApiKey", alias = "mcp_api_key")]
    pub mcp_api_key: String,

    /// Логин пользователя 1С. Используется в ДВУХ местах:
    ///   1. Basic Auth к HTTP-сервису MCP (`Authorization: Basic ...`).
    ///   2. IBCMD `--user=...` для команд `config export` / `config save` /
    ///      расширений — авторизация в самой ИБ 1С.
    /// Это один и тот же пользователь ИБ — оба канала идут через Apache на сервер 1С.
    pub login: String,

    /// Пароль пользователя 1С — см. `login`.
    pub password: String,

    // ── 1С: платформа ───────────────────────────────────────────────────────
    /// Путь к `ibcmd.exe`.
    #[serde(rename = "ibcmdPath", alias = "ibcmd_path")]
    pub ibcmd_path: String,

    // ── Output / Git ────────────────────────────────────────────────────────
    /// Каталог выгрузки = он же git-репо. Туда пишет ibcmd, оттуда делается git push.
    #[serde(rename = "outputPath", alias = "output_path")]
    pub output_path: String,

    /// Способ git-аутентификации: "domain" (Credential Manager / git helper) или "password".
    #[serde(default = "default_git_auth", rename = "gitAuthType", alias = "git_auth_type")]
    pub git_auth_type: String,

    /// Логин git для `gitAuthType = "password"`.
    #[serde(default, rename = "gitUser", alias = "git_user")]
    pub git_user: Option<String>,

    /// Пароль git для `gitAuthType = "password"`.
    #[serde(default, rename = "gitPassword", alias = "git_password")]
    pub git_password: Option<String>,

    /// URL удалённого git-репозитория (origin) для «Git commit && push».
    /// Per-base: у каждой базы свой репозиторий выгрузки со своим origin.
    /// Пусто — origin должен быть настроен в репозитории заранее (git remote add).
    #[serde(default, rename = "gitRemoteUrl", alias = "git_remote_url")]
    pub git_remote_url: String,

    /// Значение `core.autocrlf` для git-команд программы в этом репозитории.
    /// По умолчанию "false": файлы хранятся так, как их выдал ibcmd, без
    /// перекодировки концов строк. Допустимо "true", "input" и пустая строка
    /// (параметр не передаётся — действует настройка машины).
    #[serde(default = "default_git_autocrlf", rename = "gitAutocrlf", alias = "git_autocrlf")]
    pub git_autocrlf: String,

    // ── Флаги выгрузки ──────────────────────────────────────────────────────
    /// Выгружать основную конфигурацию.
    #[serde(default = "default_true", rename = "exportBase", alias = "export_base")]
    pub export_base: bool,

    /// Выгружать расширения.
    #[serde(default = "default_true", rename = "exportExtensions", alias = "export_extensions")]
    pub export_extensions: bool,

    /// Выгружать справочник ДополнительныеОтчетыИОбработки (через MSSQL).
    #[serde(default, rename = "exportProcessings", alias = "export_processings")]
    pub export_processings: bool,

    /// Сохранять бинарные снимки `_artifacts/base.cf` и `_artifacts/extensions/<имя>.cfe`
    /// через `ibcmd config save`. По умолчанию выключено — снимки большие и нужны
    /// только для развёртывания на стенд через `ibcmd config load`.
    #[serde(default, rename = "saveArtifacts", alias = "save_artifacts")]
    pub save_artifacts: bool,

    /// Инкрементальный режим IBCMD `--sync` для основной конфигурации.
    #[serde(default = "default_true", rename = "ibcmdSync", alias = "ibcmd_sync")]
    pub ibcmd_sync: bool,

    /// Инкрементальная выгрузка расширений по hash из git.
    #[serde(default = "default_true", rename = "ibcmdIncremental", alias = "ibcmd_incremental")]
    pub ibcmd_incremental: bool,

    /// Инкрементальная выгрузка допобработок (true) или полная перезапись (false,
    /// с предварительной чисткой External/ целиком). Аналог ibcmdIncremental.
    #[serde(default = "default_true", rename = "processingsIncremental", alias = "processings_incremental")]
    pub processings_incremental: bool,

    /// Количество потоков IBCMD (0 = автоматически).
    #[serde(default, rename = "ibcmdJobs", alias = "ibcmd_jobs")]
    pub ibcmd_jobs: Option<u32>,

    /// Phase 6.1: запускать `git gc` после успешного push.
    /// Без этого .git/ растёт линейно с каждым циклом (loose objects + pack
    /// фрагментация). Дефолт false — opt-in.
    #[serde(default, rename = "gitGcAfterPush", alias = "git_gc_after_push")]
    pub git_gc_after_push: bool,

    /// Использовать `git gc --aggressive --prune=now` (true, deep repack
    /// с большим окном deltify) или дешёвый `git gc --auto` (false).
    /// Aggressive держит pack плотным, но 10-60× медленнее на больших репо.
    /// Применяется только если `git_gc_after_push = true`.
    #[serde(default, rename = "gitGcAggressive", alias = "git_gc_aggressive")]
    pub git_gc_aggressive: bool,
}

// ── Default-функции для serde ────────────────────────────────────────────

fn default_check_interval_minutes() -> u64 { 30 }
fn default_lookback_first_run() -> u64 { 168 }
fn default_refetch_storage() -> u64 { 30 }
fn default_state_dir() -> String { "./state".to_string() }
fn default_log_dir() -> String { "./logs".to_string() }
fn default_log_level() -> String { "info".to_string() }
fn default_git_auth() -> String { "domain".to_string() }
fn default_git_autocrlf() -> String { "false".to_string() }
fn default_true() -> bool { true }
fn default_trigger_events() -> Vec<String> {
    vec![
        "_$InfoBase$_.DBConfigUpdate".to_string(),
        "_$InfoBase$_.DBConfigExtensionInsert".to_string(),
        "_$InfoBase$_.DBConfigExtensionUpdate".to_string(),
        "_$InfoBase$_.DBConfigExtensionDelete".to_string(),
    ]
}

/// Допустимо ли значение `gitAutocrlf`: `false`, `true`, `input`
/// или пустая строка (параметр git не передаётся).
pub fn is_valid_git_autocrlf(value: &str) -> bool {
    matches!(value.trim(), "" | "false" | "true" | "input")
}

// ── Чтение и валидация ───────────────────────────────────────────────────

impl DaemonConfig {
    /// Загрузить из файла. Делает базовую валидацию: bases непуст,
    /// alias-ы уникальны.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("не удалось прочитать {}: {}", path.display(), e))?;
        let cfg: DaemonConfig = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("ошибка парсинга {}: {}", path.display(), e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.bases.is_empty() {
            anyhow::bail!("bases[] пустой — нечего наблюдать");
        }
        let mut seen = std::collections::HashSet::new();
        for b in &self.bases {
            if b.alias.trim().is_empty() {
                anyhow::bail!("найдена запись bases[] с пустым alias");
            }
            if !seen.insert(b.alias.clone()) {
                anyhow::bail!("alias '{}' встречается дважды в bases[]", b.alias);
            }
            if b.output_path.trim().is_empty() {
                anyhow::bail!("alias '{}': outputPath пустой (это и каталог выгрузки, и git-репо)", b.alias);
            }
            if b.sql_server.trim().is_empty() {
                anyhow::bail!("alias '{}': sqlServer пустой", b.alias);
            }
            // HTTP-сервис нужен только режиму eventlog; режиму sql достаточно доступа к СУБД.
            if b.change_detection == ChangeDetection::Eventlog
                && (b.mcp_url.trim().is_empty() || b.mcp_api_key.trim().is_empty())
            {
                anyhow::bail!(
                    "alias '{}': changeDetection=eventlog требует mcpUrl и mcpApiKey",
                    b.alias
                );
            }
            if b.ibcmd_path.trim().is_empty() {
                anyhow::bail!("alias '{}': ibcmdPath пустой", b.alias);
            }
            if b.login.trim().is_empty() {
                anyhow::bail!("alias '{}': login пустой", b.alias);
            }
            if b.git_auth_type == "password"
                && (b.git_user.is_none() || b.git_password.is_none())
            {
                anyhow::bail!(
                    "alias '{}': git_auth_type=password требует git_user и git_password",
                    b.alias
                );
            }
            if !is_valid_git_autocrlf(&b.git_autocrlf) {
                anyhow::bail!(
                    "alias '{}': gitAutocrlf='{}' — допустимы false, true, input или пустая строка",
                    b.alias,
                    b.git_autocrlf
                );
            }
            if !b.ibcmd_db_auth_windows
                && (b.db_user.is_none() || b.db_pwd.is_none())
            {
                anyhow::bail!(
                    "alias '{}': ibcmd_db_auth_windows=false требует db_user и db_pwd",
                    b.alias
                );
            }
        }
        Ok(())
    }
}

// ── Поиск алиаса базы по каталогу выгрузки ───────────────────────────────

/// Путь к `bases.json`: текущий каталог, `deploy/`, каталог рядом с exe.
/// Порядок тот же, что и у GUI при подгрузке реестра баз.
pub fn find_bases_file() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("bases.json"),
        PathBuf::from("deploy/bases.json"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("bases.json"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Приведение пути к сравнимому виду: единые разделители, без хвостового слэша,
/// без учёта регистра (Windows-пути в bases.json и в config.json пишут по-разному).
fn normalize_output_path(p: &str) -> String {
    p.trim()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Алиас базы, чей `outputPath` совпадает с заданным каталогом. Поиск в готовом списке.
pub fn alias_for_output_path_in(bases: &[BaseEntry], output_path: &str) -> Option<String> {
    let target = normalize_output_path(output_path);
    if target.is_empty() {
        return None;
    }
    bases
        .iter()
        .find(|b| normalize_output_path(&b.output_path) == target)
        .map(|b| b.alias.clone())
}

/// Алиас базы по каталогу выгрузки, из `bases.json` (см. `find_bases_file`).
/// Нужен, чтобы ручной запуск (GUI/CLI) вёл состояние в state.db под тем же
/// ключом, что и служба watch, — иначе инкремент расширений и допобработок
/// сравнивается с чужим набором и выгружает всё заново.
pub fn alias_for_output_path(output_path: &str) -> Option<String> {
    let path = find_bases_file()?;
    let cfg = DaemonConfig::load(&path).ok()?;
    alias_for_output_path_in(&cfg.bases, output_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_base_json() -> &'static str {
        r#"{
            "alias": "ut",
            "sqlServer": "sql-server",
            "sqlDatabase": "ut",
            "login": "export_user",
            "password": "",
            "mcpUrl": "http://1c-server/ut/hs/mcp",
            "mcpApiKey": "key123",
            "ibcmdPath": "C:/Program Files/1cv8/8.3/bin/ibcmd.exe",
            "outputPath": "C:/Repos/demo-ut"
        }"#
    }

    #[test]
    fn parses_minimal() {
        let json = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        cfg.validate().unwrap();

        assert_eq!(cfg.check_interval_minutes, 30);
        assert_eq!(cfg.lookback_hours_first_run, 168);
        assert_eq!(cfg.bases.len(), 1);
        assert_eq!(cfg.bases[0].alias, "ut");
        assert!(cfg.bases[0].export_base);
        assert!(cfg.bases[0].export_extensions);
        assert!(!cfg.bases[0].export_processings);
        assert!(!cfg.bases[0].save_artifacts);
        assert_eq!(cfg.trigger_events.len(), 4);
    }

    #[test]
    fn save_artifacts_defaults_to_false_and_reads_true() {
        // Без ключа — бинарные снимки не сохраняются.
        let json = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        assert!(!cfg.bases[0].save_artifacts);

        // Явное значение читается как есть.
        let base_with_flag = minimal_base_json()
            .trim_end()
            .trim_end_matches('}')
            .to_string()
            + r#", "saveArtifacts": true }"#;
        let json2 = format!(r#"{{ "bases": [{}] }}"#, base_with_flag);
        let cfg2: DaemonConfig = serde_json::from_str(&json2).unwrap();
        assert!(cfg2.bases[0].save_artifacts);
    }

    #[test]
    fn log_level_defaults_to_info_and_reads_debug() {
        // Без ключа — "info".
        let json = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.log_level, "info");

        // Явное значение читается как есть.
        let json2 = format!(
            r#"{{ "logLevel": "debug", "bases": [{}] }}"#,
            minimal_base_json()
        );
        let cfg2: DaemonConfig = serde_json::from_str(&json2).unwrap();
        assert_eq!(cfg2.log_level, "debug");
    }

    #[test]
    fn git_autocrlf_defaults_to_false_and_validates_value() {
        // Без ключа — "false": концы строк не перекодируются.
        let json = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.bases[0].git_autocrlf, "false");

        // Допустимое значение читается как есть.
        let with_input = minimal_base_json()
            .trim_end()
            .trim_end_matches('}')
            .to_string()
            + r#", "gitAutocrlf": "input" }"#;
        let json2 = format!(r#"{{ "bases": [{}] }}"#, with_input);
        let cfg2: DaemonConfig = serde_json::from_str(&json2).unwrap();
        cfg2.validate().unwrap();
        assert_eq!(cfg2.bases[0].git_autocrlf, "input");

        // Недопустимое значение — ошибка валидации с понятным текстом.
        let with_junk = minimal_base_json()
            .trim_end()
            .trim_end_matches('}')
            .to_string()
            + r#", "gitAutocrlf": "yes" }"#;
        let json3 = format!(r#"{{ "bases": [{}] }}"#, with_junk);
        let cfg3: DaemonConfig = serde_json::from_str(&json3).unwrap();
        let err = format!("{}", cfg3.validate().unwrap_err());
        assert!(err.contains("gitAutocrlf"), "нет имени параметра: {}", err);
        assert!(err.contains("input"), "нет перечня допустимых значений: {}", err);
    }

    #[test]
    fn alias_by_output_path_ignores_slashes_and_case() {
        let json = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();

        // Тот же каталог, записанный иначе (обратные слэши, регистр, хвостовой слэш).
        assert_eq!(
            alias_for_output_path_in(&cfg.bases, r"C:\Repos\demo-ut").as_deref(),
            Some("ut")
        );
        assert_eq!(
            alias_for_output_path_in(&cfg.bases, "c:/repos/demo-ut/").as_deref(),
            Some("ut")
        );
        // Чужой каталог и пустая строка — алиаса нет, ключ останется прежним.
        assert_eq!(alias_for_output_path_in(&cfg.bases, "C:/Repos/demo-bp"), None);
        assert_eq!(alias_for_output_path_in(&cfg.bases, "  "), None);
    }

    #[test]
    fn rejects_empty_bases() {
        let json = r#"{"bases": []}"#;
        let cfg: DaemonConfig = serde_json::from_str(json).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(format!("{}", err).contains("bases"));
    }

    #[test]
    fn rejects_duplicate_alias() {
        let json = format!(
            r#"{{ "bases": [{}, {}] }}"#,
            minimal_base_json(),
            minimal_base_json()
        );
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(format!("{}", err).contains("'ut'"));
    }

    #[test]
    fn rejects_password_auth_without_creds() {
        // Подменяем git_auth_type на "password", но git_user/git_password не задаём.
        let base = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "gitAuthType": "password""#,
            1,
        );
        let json = format!(r#"{{ "bases": [{}] }}"#, base);
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(format!("{}", err).contains("git_user"));
    }

    /// Запись без `mcpUrl` — режим sql по умолчанию, HTTP-сервис не требуется.
    fn sql_base_json() -> &'static str {
        r#"{
            "alias": "ut",
            "sqlServer": "sql-server",
            "sqlDatabase": "ut",
            "login": "export_user",
            "password": "",
            "ibcmdPath": "C:/Program Files/1cv8/8.3/bin/ibcmd.exe",
            "outputPath": "C:/Repos/demo-ut"
        }"#
    }

    #[test]
    fn change_detection_defaults_to_sql() {
        let json = format!(r#"{{ "bases": [{}] }}"#, sql_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.bases[0].change_detection, ChangeDetection::Sql);
        assert_eq!(cfg.bases[0].mcp_url, "");
        // Без mcpUrl/mcpApiKey запись в режиме sql валидна.
        cfg.validate().unwrap();
    }

    #[test]
    fn change_detection_parses_both_key_forms() {
        let base = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "changeDetection": "eventlog""#,
            1,
        );
        let json = format!(r#"{{ "bases": [{}] }}"#, base);
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.bases[0].change_detection, ChangeDetection::Eventlog);
        cfg.validate().unwrap();

        // snake_case-alias ключа и значение "sql".
        let base2 = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "change_detection": "sql""#,
            1,
        );
        let json2 = format!(r#"{{ "bases": [{}] }}"#, base2);
        let cfg2: DaemonConfig = serde_json::from_str(&json2).unwrap();
        assert_eq!(cfg2.bases[0].change_detection, ChangeDetection::Sql);
    }

    #[test]
    fn rejects_eventlog_without_mcp_url() {
        let base = sql_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "changeDetection": "eventlog""#,
            1,
        );
        let json = format!(r#"{{ "bases": [{}] }}"#, base);
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(format!("{}", err).contains("changeDetection=eventlog"));
    }

    #[test]
    fn git_gc_fields_default_false() {
        // Минимальный JSON без явных gitGc* — оба поля дефолтятся в false.
        let json = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        let base = &cfg.bases[0];
        assert!(!base.git_gc_after_push);
        assert!(!base.git_gc_aggressive);
    }

    #[test]
    fn git_gc_fields_camel_case() {
        // Явные значения true в camelCase из JSON.
        let base = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "gitGcAfterPush": true, "gitGcAggressive": true"#,
            1,
        );
        let json = format!(r#"{{ "bases": [{}] }}"#, base);
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        let b = &cfg.bases[0];
        assert!(b.git_gc_after_push);
        assert!(b.git_gc_aggressive);
    }

    #[test]
    fn git_gc_fields_snake_case_alias() {
        // serde-alias snake_case тоже должен работать (для обратной совместимости).
        let base = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "git_gc_after_push": true, "git_gc_aggressive": false"#,
            1,
        );
        let json = format!(r#"{{ "bases": [{}] }}"#, base);
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        let b = &cfg.bases[0];
        assert!(b.git_gc_after_push);
        assert!(!b.git_gc_aggressive);
    }

    #[test]
    fn git_remote_url_camel_snake_and_roundtrip() {
        // camelCase-ключ читается.
        let base = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "gitRemoteUrl": "https://gitlab.example/repo.git""#,
            1,
        );
        let json = format!(r#"{{ "bases": [{}] }}"#, base);
        let cfg: DaemonConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.bases[0].git_remote_url, "https://gitlab.example/repo.git");

        // snake_case-alias тоже читается (обратная совместимость).
        let base2 = minimal_base_json().replacen(
            r#""outputPath": "C:/Repos/demo-ut""#,
            r#""outputPath": "C:/Repos/demo-ut", "git_remote_url": "git@host:repo.git""#,
            1,
        );
        let json2 = format!(r#"{{ "bases": [{}] }}"#, base2);
        let cfg2: DaemonConfig = serde_json::from_str(&json2).unwrap();
        assert_eq!(cfg2.bases[0].git_remote_url, "git@host:repo.git");

        // round-trip: GUI пишет через to_string_pretty — значение и camelCase-ключ переживают.
        let out = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(out.contains("\"gitRemoteUrl\""));
        let back: DaemonConfig = serde_json::from_str(&out).unwrap();
        assert_eq!(back.bases[0].git_remote_url, "https://gitlab.example/repo.git");

        // отсутствие ключа → пустая строка (default), не ошибка парсинга.
        let json3 = format!(r#"{{ "bases": [{}] }}"#, minimal_base_json());
        let cfg3: DaemonConfig = serde_json::from_str(&json3).unwrap();
        assert_eq!(cfg3.bases[0].git_remote_url, "");
    }
}
