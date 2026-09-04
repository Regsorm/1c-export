//! State per-base: `state/<alias>.json`.
//!
//! Что хранится:
//! - `last_processed_at` — timestamp последнего обработанного события (по нему
//!   делается следующий `eventlog_query` с `from = last_processed_at`).
//! - `processed_hashes_at_last_dt` — хэши событий ровно с этим timestamp,
//!   используются для де-дапа коллизий внутри секунды (см. `eventlog_watcher`).
//! - `storage_mapping` — кэш SQL-имён таблицы и полей справочника
//!   ДополнительныеОтчетыИОбработки. Получается через MCP-вызов
//!   `db_table_fields` один раз и подсовывается в выгрузку.
//! - Метрики: `consecutive_failures`, `last_export_status` и т.д.
//!
//! Запись атомарная: пишем во временный файл рядом и переименовываем.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaseState {
    pub alias: String,

    /// Timestamp (как строка, как его отдаёт MCP) последнего обработанного события.
    /// При первом запуске — None, тогда watch берёт окно lookback_hours_first_run.
    #[serde(default)]
    pub last_processed_at: Option<String>,

    /// SHA-256 хэши событий ровно с timestamp `last_processed_at`. Нужны для
    /// фильтрации повторов внутри секунды, потому что MCP возвращает события
    /// с точностью до секунды и `from=` включающий, не эксклюзивный.
    #[serde(default)]
    pub processed_hashes_at_last_dt: Vec<String>,

    /// Когда watch последний раз делал цикл по этой базе (для health-метрик).
    #[serde(default)]
    pub last_checked_at: Option<String>,

    /// Статус последней реальной выгрузки: "ok" / "fail: <message>".
    #[serde(default)]
    pub last_export_status: Option<String>,

    /// Длительность последней выгрузки (секунды).
    #[serde(default)]
    pub last_export_duration_sec: Option<u64>,

    /// Сколько подряд циклов было неуспешных. Сбрасывается на 0 при успехе.
    #[serde(default)]
    pub consecutive_failures: u32,

    /// Кэш SQL-имён для допобработок (вызов db_table_fields делается
    /// при первом запуске и раз в N дней по таймеру).
    #[serde(default)]
    pub storage_mapping: Option<StoredMapping>,

    /// Отпечатки состояния базы для режима changeDetection=sql. None — опрос ещё не делался.
    #[serde(default)]
    pub sql_signals: Option<SqlSignals>,
}

/// Отпечатки служебных таблиц MS SQL, по которым watch решает, изменилась ли база.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlSignals {
    /// Основная конфигурация: "<MAX(Modified) как строка>|<COUNT(*)>" по таблице Config.
    #[serde(default)]
    pub config: String,
    /// Расширения: SHA-256 (hex) от отсортированных строк "_ExtName|_UpdateTime|_Version"
    /// таблицы _ExtensionsInfo.
    #[serde(default)]
    pub extensions: String,
    /// Допобработки: SHA-256 (hex) от отсортированных строк "_IDRRef|hash" таблицы справочника.
    #[serde(default)]
    pub processings: String,
    /// Когда сняты (ISO-8601 UTC).
    #[serde(default)]
    pub taken_at: String,
}

/// Снимок StorageMapping с timestamp получения.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMapping {
    pub table: String,
    pub field_storage: String,
    pub field_hash: String,
    pub field_kind: String,
    /// SQL-имя таблицы перечисления `ВидыДополнительныхОтчётовИОбработок`.
    /// Нужно для классификации .epf/.erf в watch-выгрузках. В старых state-файлах
    /// отсутствует — при отсутствии остаётся пустым (fallback на .epf, см. ext_by_kind).
    #[serde(default)]
    pub enum_table: String,
    /// ISO-8601 UTC, когда был выполнен MCP-вызов db_table_fields.
    pub fetched_at: String,
}

impl BaseState {
    /// Загрузить state из `<state_dir>/<alias>.json`. Если файла нет — вернуть
    /// дефолтную пустую запись с заданным alias (это нормальный путь первого запуска).
    pub fn load(state_dir: &Path, alias: &str) -> anyhow::Result<Self> {
        let path = Self::path(state_dir, alias);
        if !path.exists() {
            return Ok(Self {
                alias: alias.to_string(),
                ..Default::default()
            });
        }
        let text = fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("чтение {}: {}", path.display(), e))?;
        let mut s: BaseState = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("парсинг {}: {}", path.display(), e))?;
        // Защита от переименования alias в файле (alias в имени файла — авторитетный).
        s.alias = alias.to_string();
        Ok(s)
    }

    /// Атомарная запись: tempfile в той же директории + rename (на Windows
    /// `rename` через MoveFileEx с replace, на POSIX — атомарный syscall).
    pub fn save(&self, state_dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(state_dir)
            .map_err(|e| anyhow::anyhow!("создание {}: {}", state_dir.display(), e))?;
        let final_path = Self::path(state_dir, &self.alias);
        let tmp_path = final_path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        fs::write(&tmp_path, text)
            .map_err(|e| anyhow::anyhow!("запись {}: {}", tmp_path.display(), e))?;
        // На Windows std::fs::rename перезаписывает целевой файл (с MoveFileExW + REPLACE_EXISTING).
        fs::rename(&tmp_path, &final_path)
            .map_err(|e| anyhow::anyhow!("rename {} → {}: {}", tmp_path.display(), final_path.display(), e))?;
        Ok(())
    }

    /// Нужно ли заново вызвать `db_table_fields` (нет mapping
    /// либо он старее N дней).
    pub fn needs_storage_refetch(&self, ttl_days: u64) -> bool {
        let Some(ref m) = self.storage_mapping else { return true; };
        let Ok(dt) = parse_iso8601(&m.fetched_at) else { return true; };
        let age_secs = (Utc::now() - dt).num_seconds().max(0) as u64;
        age_secs > ttl_days * 86400
    }

    fn path(state_dir: &Path, alias: &str) -> PathBuf {
        state_dir.join(format!("{}.json", alias))
    }
}

/// Парсит ISO-8601 даты в нескольких форматах, которые могут встретиться:
/// "2026-04-26T21:02:38" (без TZ — считаем UTC), "2026-04-26T21:02:38+03:00",
/// "2026-04-26T21:02:38Z".
fn parse_iso8601(s: &str) -> anyhow::Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .map_err(|e| anyhow::anyhow!("неизвестный формат timestamp '{}': {}", s, e))?;
    Ok(naive.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_returns_default_when_missing() {
        let dir = tempdir().unwrap();
        let s = BaseState::load(dir.path(), "demo-ut").unwrap();
        assert_eq!(s.alias, "demo-ut");
        assert!(s.last_processed_at.is_none());
        assert!(s.storage_mapping.is_none());
        assert_eq!(s.consecutive_failures, 0);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempdir().unwrap();
        let mut s = BaseState {
            alias: "ut".into(),
            last_processed_at: Some("2026-04-26T21:02:38".into()),
            processed_hashes_at_last_dt: vec!["abc".into(), "def".into()],
            consecutive_failures: 2,
            ..Default::default()
        };
        s.storage_mapping = Some(StoredMapping {
            table: "_Reference181".into(),
            field_storage: "_Fld4776".into(),
            field_hash: "_Version".into(),
            field_kind: "_Fld4766RRef".into(),
            enum_table: "_Enum1315".into(),
            fetched_at: Utc::now().to_rfc3339(),
        });
        s.save(dir.path()).unwrap();

        let loaded = BaseState::load(dir.path(), "ut").unwrap();
        assert_eq!(loaded.last_processed_at.as_deref(), Some("2026-04-26T21:02:38"));
        assert_eq!(loaded.processed_hashes_at_last_dt, vec!["abc", "def"]);
        assert_eq!(loaded.consecutive_failures, 2);
        let m = loaded.storage_mapping.as_ref().unwrap();
        assert_eq!(m.table, "_Reference181");
        assert_eq!(m.field_hash, "_Version");
    }

    #[test]
    fn sql_signals_roundtrip_and_missing_field() {
        let dir = tempdir().unwrap();
        let s = BaseState {
            alias: "ut".into(),
            sql_signals: Some(SqlSignals {
                config: "2026-09-04 10:00:00.000|1234".into(),
                extensions: "aa11".into(),
                processings: "bb22".into(),
                taken_at: "2026-09-04T10:00:00+00:00".into(),
            }),
            ..Default::default()
        };
        s.save(dir.path()).unwrap();
        let loaded = BaseState::load(dir.path(), "ut").unwrap();
        assert_eq!(loaded.sql_signals.as_ref().unwrap().extensions, "aa11");
        assert_eq!(loaded.sql_signals.as_ref().unwrap().config, "2026-09-04 10:00:00.000|1234");

        // Старый state-файл без поля sql_signals читается, отпечатков просто нет.
        std::fs::write(dir.path().join("old.json"), r#"{"alias":"old"}"#).unwrap();
        let old = BaseState::load(dir.path(), "old").unwrap();
        assert!(old.sql_signals.is_none());
    }

    #[test]
    fn needs_refetch_when_no_mapping() {
        let s = BaseState::default();
        assert!(s.needs_storage_refetch(30));
    }

    #[test]
    fn needs_refetch_when_old() {
        let s = BaseState {
            storage_mapping: Some(StoredMapping {
                table: "x".into(), field_storage: "x".into(),
                field_hash: "x".into(), field_kind: "x".into(),
                enum_table: "x".into(),
                // 100 дней назад
                fetched_at: (Utc::now() - chrono::Duration::days(100)).to_rfc3339(),
            }),
            ..Default::default()
        };
        assert!(s.needs_storage_refetch(30));
    }

    #[test]
    fn no_refetch_when_fresh() {
        let s = BaseState {
            storage_mapping: Some(StoredMapping {
                table: "x".into(), field_storage: "x".into(),
                field_hash: "x".into(), field_kind: "x".into(),
                enum_table: "x".into(),
                fetched_at: Utc::now().to_rfc3339(),
            }),
            ..Default::default()
        };
        assert!(!s.needs_storage_refetch(30));
    }
}
