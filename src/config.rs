use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use crate::error::ExportError;

/// Тип аутентификации
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    Os,
    Password,
}

/// Настройки аутентификации
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(rename = "type")]
    pub auth_type: AuthType,
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub password: String,
}

/// Основная конфигурация приложения
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Сервер СУБД (MSSQL). Используется для прямого TDS-коннекта при выгрузке
    /// допобработок (tiberius) и для IBCMD `--db-server=`.
    /// JSON: `"sqlServer"` (alias для совместимости — `"server"`).
    #[serde(rename = "sqlServer", alias = "server")]
    pub server: String,
    /// Сервер кластера 1С (для ENTERPRISE/DESIGNER `/S server\db` и IBCMD `--ibconnection=Srvr=`).
    /// Если пусто — используется значение `server` (для случая когда СУБД и 1С на одной машине).
    #[serde(default, rename = "server1C", alias = "server_1c")]
    pub server_1c: String,
    /// Имя информационной базы 1С в кластере. В watch-пути НЕ используется
    /// (там вся связь с 1С через `mcp_url`). Нужно только в legacy CLI-сценариях
    /// — Designer-дамп .epf, ENTERPRISE-автодискавери.
    #[serde(default)]
    pub database: String,
    /// Имя физической БД на MSSQL — для прямого TDS-коннекта (выгрузка допобработок)
    /// и для IBCMD `--db-name=` при подключении напрямую к СУБД. Если не задано —
    /// fallback на `database` (для случаев когда имена совпадают).
    #[serde(default, rename = "sqlDatabase", alias = "sql_database")]
    pub sql_database: String,
    pub authentication: AuthConfig,
    /// Путь к ibcmd.exe. В каталоге рядом штатно лежит и 1cv8.exe — он используется
    /// только в редких CLI-кейсах (Designer-fallback автодискавери в processings,
    /// дамп .epf/.erf в Designer-команде). В watch-пути 1cv8.exe не запускается никогда.
    #[serde(rename = "ibcmdPath", alias = "ibcmd_path")]
    pub ibcmd_path: String,
    /// Полный URL HTTP-сервиса MCP в самой 1С — например
    /// `http://localhost/demo-ut/hs/mcp` (если демон стоит на сервере 1С) или
    /// `http://1c-server/demo-ut/hs/mcp`. Watch шлёт сюда `tools/call`-запросы
    /// (`eventlog_query`, `db_table_fields`).
    #[serde(default, rename = "mcpUrl", alias = "mcp_url")]
    pub mcp_url: String,
    /// API-ключ HTTP-сервиса MCP в 1С (заголовок `X-MCP-Key`).
    #[serde(default, rename = "mcpApiKey", alias = "mcp_api_key")]
    pub mcp_api_key: String,
    /// Полное имя справочника в метаданных 1С, который содержит дополнительные
    /// отчёты и обработки БСП. Нужен и для параметра `table` в `db_table_fields`,
    /// и для парсинга ответа. По умолчанию "Справочник.ДополнительныеОтчетыИОбработки".
    /// Вынесено в конфиг на случай ребрендинга в будущих версиях БСП.
    #[serde(default, rename = "processingsMetaName", alias = "processings_meta_name")]
    pub processings_meta_name: String,
    /// URL удалённого git-репозитория (origin) для «Git commit & push» из GUI.
    /// Пусто — origin должен быть настроен в репозитории выгрузки заранее.
    #[serde(default, rename = "gitRemoteUrl", alias = "git_remote_url")]
    pub git_remote_url: String,
    /// Значение `core.autocrlf` для git-команд программы в репозитории выгрузки.
    /// По умолчанию "false": файлы хранятся так, как их выдал ibcmd, без
    /// перекодировки концов строк. Допустимо "true", "input" и пустая строка
    /// (параметр не передаётся — действует настройка машины).
    #[serde(default = "default_git_autocrlf", rename = "gitAutocrlf", alias = "git_autocrlf")]
    pub git_autocrlf: String,
    #[serde(rename = "outputPath")]
    pub output_path: String,
    /// Подробность журнала: "info" (дефолт) или "debug". Неизвестное значение
    /// трактуется как "info" — отдельной валидации нет.
    #[serde(default = "default_log_level", rename = "logLevel", alias = "log_level")]
    pub log_level: String,
    /// Сохранять бинарные снимки `_artifacts/base.cf` и `_artifacts/extensions/<имя>.cfe`
    /// через `ibcmd config save`. По умолчанию выключено.
    #[serde(default, rename = "saveArtifacts", alias = "save_artifacts")]
    pub save_artifacts: bool,
    #[serde(default)]
    pub extensions: Vec<String>,
}

fn default_log_level() -> String { "info".to_string() }
fn default_git_autocrlf() -> String { "false".to_string() }

impl AppConfig {
    /// Построить AppConfig из BaseEntry — все настройки watch-режима лежат там одной
    /// плоской структурой. Используется в watch-пути, чтобы существующий код
    /// (ExportCoordinator, command_builder, processings) работал без изменений.
    pub fn from_base(b: &crate::bases_config::BaseEntry) -> Self {
        Self {
            server: b.sql_server.clone(),
            server_1c: b.server_1c.clone(),
            database: String::new(),
            sql_database: b.sql_database.clone(),
            authentication: AuthConfig {
                auth_type: AuthType::Password,
                login: b.login.clone(),
                password: b.password.clone(),
            },
            ibcmd_path: b.ibcmd_path.clone(),
            mcp_url: b.mcp_url.clone(),
            mcp_api_key: b.mcp_api_key.clone(),
            // processings_meta_name теперь глобальный (DaemonConfig) — резолвим в watch.rs
            // при вызове fetch_storage_mapping. Здесь оставляем пустым.
            processings_meta_name: String::new(),
            // origin per-base хранится в bases.json (BaseEntry.git_remote_url).
            git_remote_url: b.git_remote_url.clone(),
            git_autocrlf: b.git_autocrlf.clone(),
            output_path: b.output_path.clone(),
            // Уровень журнала в watch-режиме берётся из DaemonConfig (bases.json).
            log_level: default_log_level(),
            save_artifacts: b.save_artifacts,
            extensions: Vec::new(),
        }
    }

    /// Загрузка конфигурации из JSON файла
    pub fn load(path: &Path) -> Result<Self, ExportError> {
        if !path.exists() {
            return Err(ExportError::FileNotFound(path.display().to_string()));
        }
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;
        Ok(config)
    }

    /// Загрузка конфигурации с автоопределением пути
    /// Ищет config/config.json рядом с исполняемым файлом
    pub fn load_auto() -> Result<Self, ExportError> {
        let exe_dir = std::env::current_exe()
            .map(|p| p.parent().unwrap_or(Path::new(".")).to_path_buf())
            .unwrap_or_else(|_| PathBuf::from("."));

        let config_path = exe_dir.join("config").join("config.json");
        if config_path.exists() {
            return Self::load(&config_path);
        }

        // Пробуем относительно текущей директории
        let cwd_config = PathBuf::from("config").join("config.json");
        if cwd_config.exists() {
            return Self::load(&cwd_config);
        }

        Err(ExportError::Config(
            "Файл конфигурации config/config.json не найден. Укажите путь через --config".to_string()
        ))
    }

    /// Валидация конфигурации
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.server.is_empty() {
            errors.push("Не указан SQL-сервер (sqlServer)".to_string());
        }
        // database (имя ИБ 1С) НЕ валидируем — оно опционально, нужно только legacy CLI-сценариям.
        if self.ibcmd_path.is_empty() {
            errors.push("Не указан путь к ibcmd.exe (ibcmd_path)".to_string());
        } else if !Path::new(&self.ibcmd_path).exists() {
            errors.push(format!("ibcmd.exe не найден: {}", self.ibcmd_path));
        }

        if !crate::bases_config::is_valid_git_autocrlf(&self.git_autocrlf) {
            errors.push(format!(
                "Недопустимое значение gitAutocrlf: '{}' — допустимы false, true, input или пустая строка",
                self.git_autocrlf
            ));
        }

        if let AuthType::Password = self.authentication.auth_type {
            if self.authentication.login.is_empty() {
                errors.push("Не указан логин (authentication.login)".to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Сервер для legacy CLI-операций через платформу 1С (Designer-дамп .epf,
    /// ENTERPRISE-автодискавери в processings). В watch-пути НЕ используется —
    /// там вся связь с 1С через `mcp_url` (HTTP-сервис).
    /// Fallback: `server_1c` → `server`.
    pub fn server_for_1c(&self) -> &str {
        if !self.server_1c.is_empty() {
            &self.server_1c
        } else {
            &self.server
        }
    }

    /// Полное имя справочника допобработок в метаданных 1С — с fallback на дефолт.
    pub fn processings_meta_name(&self) -> &str {
        if self.processings_meta_name.is_empty() {
            "Справочник.ДополнительныеОтчетыИОбработки"
        } else {
            &self.processings_meta_name
        }
    }

    /// Имя БД на MSSQL для прямого SQL-коннекта (TDS) и для IBCMD `--db-name=`.
    /// Если `sql_database` пуст — fallback на `database` (имя ИБ 1С), для случаев
    /// когда имена совпадают и оператор не хочет дублировать поле.
    pub fn sql_database_name(&self) -> &str {
        if !self.sql_database.is_empty() {
            &self.sql_database
        } else {
            &self.database
        }
    }

    /// Путь к ibcmd.exe (просто PathBuf от конфигурационного поля).
    /// Существование файла проверяется в `validate()`.
    pub fn ibcmd_path(&self) -> Result<PathBuf, ExportError> {
        let p = PathBuf::from(&self.ibcmd_path);
        if !p.exists() {
            return Err(ExportError::IbcmdNotFound(format!(
                "ibcmd.exe не найден: {}",
                self.ibcmd_path
            )));
        }
        Ok(p)
    }

    /// Путь к 1cv8.exe — резолвится симметрично: тот же каталог, что у ibcmd.exe.
    /// Используется только в legacy-сценариях:
    ///   - CLI `--export-processings` без override-флагов и без кэша `_manifest.json`
    ///     (разовый автодискавери через `1cv8.exe ENTERPRISE /C BatchGet…`),
    ///   - Designer-команды дампа .epf/.erf (`designer_dump_epf`).
    /// В watch-пути не вызывается никогда.
    pub fn platform_1cv8_path(&self) -> Result<PathBuf, ExportError> {
        let ibcmd = Path::new(&self.ibcmd_path);
        let bin_dir = ibcmd.parent().ok_or_else(|| {
            ExportError::IbcmdNotFound(format!(
                "Не удалось определить каталог платформы из ibcmd_path={}",
                self.ibcmd_path
            ))
        })?;
        let exe = bin_dir.join("1cv8.exe");
        if !exe.exists() {
            return Err(ExportError::IbcmdNotFound(format!(
                "1cv8.exe не найден в {}",
                bin_dir.display()
            )));
        }
        Ok(exe)
    }
}
