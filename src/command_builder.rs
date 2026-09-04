use std::path::{Path, PathBuf};
use crate::config::{AppConfig, AuthType};

/// Тип авторизации на СУБД MSSQL для IBCMD
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IbcmdDbAuth {
    /// Доменная (Windows-интегрированная) — --db-user/--db-pwd не передаются
    Windows,
    /// Логин/пароль SQL-пользователя
    SqlLogin,
}

/// Параметры IBCMD (передаются из CLI или GUI)
#[derive(Clone)]
pub struct IbcmdParams {
    pub ibcmd_path: PathBuf,
    pub dbms: String,
    /// Тип авторизации на СУБД: доменная или SQL-логин
    pub db_auth: IbcmdDbAuth,
    pub db_user: Option<String>,
    pub db_pwd: Option<String>,
    pub use_connection_string: bool,
    /// Количество потоков выгрузки (--threads). 0 = не указывать.
    /// У ibcmd config export параметр называется --threads / -T (не --jobs).
    pub jobs: u32,
    /// Инкрементальная синхронизация XML-дампа (--sync).
    /// Работает только с `config export` (и --extension=<N>); у `all-extensions` не поддерживается.
    pub sync: bool,
    /// Флаг --force. При --sync разрешает полный дамп если ConfigDumpInfo.xml несовместим.
    /// Без --sync потенциально разрешает перезапись непустой папки (поведение не документировано).
    pub force: bool,
    /// Инкрементальная выгрузка расширений: сравнивать hash-sum из `config extension list`
    /// с прошлыми хэшами (читаются через git show HEAD:.extensions-hashes.json),
    /// выгружать только изменившиеся. Если false — полная перезапись папки extensions/.
    pub incremental_extensions: bool,
}

/// Построение команд IBCMD
pub struct IbcmdBuilder;

impl IbcmdBuilder {
    /// Базовая часть: `ibcmd infobase <subgroup>`
    fn base(params: &IbcmdParams, subgroup: &str) -> Vec<String> {
        vec![
            params.ibcmd_path.to_string_lossy().to_string(),
            "infobase".to_string(),
            subgroup.to_string(),
        ]
    }

    /// Параметры подключения к СУБД (без авторизации в ИБ).
    /// Используется командами, работающими без открытия сеанса в ИБ
    /// (напр. `extension list`).
    fn add_db_connection(cmd: &mut Vec<String>, params: &IbcmdParams, config: &AppConfig) {
        if params.use_connection_string {
            cmd.push(format!("--ibconnection=Srvr={};Ref={}", config.server_for_1c(), config.database));
        } else {
            cmd.push(format!("--db-server={}", config.server));
            cmd.push(format!("--dbms={}", params.dbms));
            cmd.push(format!("--db-name={}", config.sql_database_name()));
            if params.db_auth == IbcmdDbAuth::SqlLogin {
                if let Some(ref user) = params.db_user {
                    cmd.push(format!("--db-user={}", user));
                }
                if let Some(ref pwd) = params.db_pwd {
                    cmd.push(format!("--db-pwd={}", pwd));
                }
            }
        }
    }

    /// Параметры подключения к СУБД + авторизации в ИБ 1С.
    /// Для команд `config export` / `config save`, требующих сеанса в ИБ.
    fn add_connection(cmd: &mut Vec<String>, params: &IbcmdParams, config: &AppConfig) {
        Self::add_db_connection(cmd, params, config);

        if let AuthType::Password = config.authentication.auth_type {
            if !config.authentication.login.is_empty() {
                cmd.push(format!("--user={}", config.authentication.login));
            }
            if !config.authentication.password.is_empty() {
                cmd.push(format!("--password={}", config.authentication.password));
            }
        }
    }

    fn add_threads(cmd: &mut Vec<String>, params: &IbcmdParams) {
        if params.jobs > 0 {
            cmd.push(format!("--threads={}", params.jobs));
        }
    }

    /// Выгрузка основной конфигурации: `config export` (XML).
    /// --sync и --force добавляются по флагам из params.
    pub fn export_base(params: &IbcmdParams, config: &AppConfig) -> Vec<String> {
        let mut cmd = Self::base(params, "config");
        let output = Path::new(&config.output_path).join("base");

        cmd.push("export".to_string());
        Self::add_connection(&mut cmd, params, config);
        Self::add_threads(&mut cmd, params);
        if params.sync {
            cmd.push("--sync".to_string());
        }
        if params.force {
            cmd.push("--force".to_string());
        }
        cmd.push(output.to_string_lossy().to_string());
        cmd
    }

    /// Выгрузка одного расширения: `config export --extension=<name>`.
    /// Расширения всегда перезаписываются полностью (без --sync).
    pub fn export_extension(params: &IbcmdParams, config: &AppConfig, ext_name: &str) -> Vec<String> {
        let mut cmd = Self::base(params, "config");
        let output = Path::new(&config.output_path).join("extensions").join(ext_name);

        cmd.push("export".to_string());
        cmd.push(format!("--extension={}", ext_name));
        Self::add_connection(&mut cmd, params, config);
        Self::add_threads(&mut cmd, params);
        cmd.push(output.to_string_lossy().to_string());
        cmd
    }

    /// Получение списка расширений инфобазы: `ibcmd infobase config extension list`.
    /// Группа `extension` находится ВНУТРИ `config`, а не на верхнем уровне `infobase`.
    /// Команда ТРЕБУЕТ авторизации в ИБ (`--user`/`--password`) — если их нет,
    /// ibcmd запрашивает их интерактивно и «висит» на чтении stdin.
    pub fn list_extensions(params: &IbcmdParams, config: &AppConfig) -> Vec<String> {
        let mut cmd = Self::base(params, "config");
        cmd.push("extension".to_string());
        cmd.push("list".to_string());
        Self::add_connection(&mut cmd, params, config);
        cmd
    }

    /// Бинарная выгрузка основной конфигурации в .cf-файл: `config save <позиционный_путь>`.
    /// Используется параллельно с `export_base` (XML) — даёт надёжный родной snapshot
    /// для деплоя на тех-стенд через `ibcmd infobase config load`. Файл кладём
    /// в `<output>/_artifacts/base.cf` (Git LFS-фильтр в .gitattributes).
    ///
    /// ВАЖНО (эмпирические факты, проверено 2026-05-03 на ibcmd 8.3.27.1786):
    /// - `--threads=N` НЕ принимается (выдаёт «Ошибка разбора параметра», exit=2).
    /// - `--file=<путь>` тоже НЕ принимается. Путь — ТОЛЬКО позиционный аргумент в конце.
    /// См. карточка #1154 в обеих базах знаний.
    pub fn save_base(params: &IbcmdParams, config: &AppConfig) -> Vec<String> {
        let mut cmd = Self::base(params, "config");
        let output = Path::new(&config.output_path)
            .join("_artifacts")
            .join("base.cf");

        cmd.push("save".to_string());
        Self::add_connection(&mut cmd, params, config);
        cmd.push(output.to_string_lossy().to_string());
        cmd
    }

    /// Бинарная выгрузка одного расширения в .cfe-файл:
    /// `config save --extension=<name> <output>/_artifacts/extensions/<name>.cfe`.
    /// Путь — позиционный аргумент (так и для base, и для extension — `--file=` не работает).
    /// `--threads` не поддерживается командой `config save` (см. save_base).
    pub fn save_extension(params: &IbcmdParams, config: &AppConfig, ext_name: &str) -> Vec<String> {
        let mut cmd = Self::base(params, "config");
        let output = Path::new(&config.output_path)
            .join("_artifacts")
            .join("extensions")
            .join(format!("{}.cfe", ext_name));

        cmd.push("save".to_string());
        cmd.push(format!("--extension={}", ext_name));
        Self::add_connection(&mut cmd, params, config);
        cmd.push(output.to_string_lossy().to_string());
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, AuthType};

    fn mk_params() -> IbcmdParams {
        IbcmdParams {
            ibcmd_path: PathBuf::from(r"C:\Program Files\1cv8\8.3.27.1786\bin\ibcmd.exe"),
            dbms: "MSSQLServer".to_string(),
            db_auth: IbcmdDbAuth::Windows,
            db_user: None,
            db_pwd: None,
            use_connection_string: false,
            jobs: 8,
            sync: false,
            force: false,
            incremental_extensions: true,
        }
    }

    fn mk_config(output: &str) -> AppConfig {
        AppConfig {
            server: "sql-server".to_string(),
            server_1c: String::new(),
            database: "demo-ut".to_string(),
            sql_database: "demo-ut".to_string(),
            authentication: AuthConfig {
                auth_type: AuthType::Password,
                login: "export_user".to_string(),
                password: String::new(),
            },
            ibcmd_path: r"C:\Program Files\1cv8\8.3.27.1786\bin\ibcmd.exe".to_string(),
            mcp_url: String::new(),
            mcp_api_key: String::new(),
            processings_meta_name: String::new(),
            git_remote_url: String::new(),
            git_autocrlf: "false".to_string(),
            output_path: output.to_string(),
            log_level: "info".to_string(),
            save_artifacts: true,
            extensions: Vec::new(),
        }
    }

    #[test]
    fn save_base_builds_correct_cmd() {
        let params = mk_params();
        let config = mk_config(r"C:\Repos\demo-ut");
        let cmd = IbcmdBuilder::save_base(&params, &config);

        // Структура: ibcmd infobase config save <connection> <позиционный_путь>
        // НИ `--threads`, НИ `--file=` НЕ должно быть — `config save` ничего из этого не принимает.
        assert_eq!(cmd[1], "infobase");
        assert_eq!(cmd[2], "config");
        assert_eq!(cmd[3], "save");
        assert!(!cmd.iter().any(|s| s.starts_with("--threads")),
            "config save не принимает --threads: {:?}", cmd);
        assert!(!cmd.iter().any(|s| s.starts_with("--file=")),
            "config save не принимает --file=, путь только позиционный: {:?}", cmd);
        assert!(cmd.iter().any(|s| s == "--user=export_user"));
        let last = cmd.last().unwrap();
        assert!(last.ends_with(r"_artifacts\base.cf") || last.ends_with("_artifacts/base.cf"),
            "позиционный путь к base.cf: {}", last);
        assert!(!cmd.iter().any(|s| s.starts_with("--extension")), "save_base не должен содержать --extension");
    }

    #[test]
    fn save_extension_builds_correct_cmd() {
        let params = mk_params();
        let config = mk_config(r"C:\Repos\demo-ut");
        let cmd = IbcmdBuilder::save_extension(&params, &config, "ДоработкаУТ");

        assert_eq!(cmd[1], "infobase");
        assert_eq!(cmd[2], "config");
        assert_eq!(cmd[3], "save");
        assert!(cmd.iter().any(|s| s == "--extension=ДоработкаУТ"),
            "должен быть --extension=ДоработкаУТ: {:?}", cmd);
        assert!(!cmd.iter().any(|s| s.starts_with("--threads")),
            "config save --extension тоже не принимает --threads: {:?}", cmd);
        // Путь — позиционный аргумент (последний), не --file=
        let last = cmd.last().unwrap();
        assert!(last.ends_with(r"_artifacts\extensions\ДоработкаУТ.cfe")
                || last.ends_with("_artifacts/extensions/ДоработкаУТ.cfe"),
            "позиционный путь к .cfe: {}", last);
        assert!(!cmd.iter().any(|s| s.starts_with("--file=")),
            "save_extension использует позиционный путь, не --file=");
    }
}
