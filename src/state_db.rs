//! Локальная SQLite-база состояния дельты и истории выгрузок.
//!
//! Одна общая БД на все репозитории (`state.db`), лежит рядом с exe — вне
//! выгружаемых папок и вне git. Данные разных баз изолированы колонкой `repo`
//! (= alias базы). Удаление файла БД = полная перевыгрузка всех баз с нуля.
//!
//! Содержит:
//! - `ext_hashes`   — хеши расширений (имя → hash-sum), для инкремента;
//! - `proc_items`   — хеши допобработок (uuid → MD5/rowversion), для инкремента;
//! - `proc_discovery` — кэш SQL-имён справочника ДопОбработок + маппинг видов;
//! - `export_log`   — история выгрузок (append-only), для просмотра в GUI.
//!
//! Раньше всё это (кроме лога — его не было) лежало в коммитимых файлах
//! `extensions/.extensions-hashes.json` и `external/_manifest.json` внутри репо
//! выгрузки и засоряло git-историю. См. карточки #1244 / #1272.
//!
//! Watch-состояние (`state/<alias>.json`: закладка журнала, метрики) сюда НЕ
//! переносится — оно и так лежит вне git и не накапливается.

use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Запись об одной допобработке (зеркало `processings::ManifestItem` без serde,
/// чтобы не завязывать слой БД на типы модуля выгрузки).
#[derive(Debug, Clone, PartialEq)]
pub struct ProcItem {
    pub name: String,
    pub kind: String,
    pub hash: String,
    pub path: String,
    pub size: u64,
    pub updated: String,
}

/// Кэш структуры хранения справочника ДопОбработок + маппинг UUID видов → имя.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub table: String,
    pub field_storage: String,
    pub field_hash: String,
    pub field_kind: String,
    /// SQL-имя таблицы перечисления `ВидыДополнительныхОтчётовИОбработок`.
    pub enum_table: String,
    pub hash_is_binary: bool,
    pub discovered_at: Option<String>,
    pub kind_uuid_to_name: HashMap<String, String>,
}

/// Что записываем в журнал выгрузки (одна строка на выгрузку).
#[derive(Debug, Clone)]
pub struct ExportLogEntry {
    pub repo: String,
    /// Локальное время завершения, формат "%Y-%m-%d %H:%M:%S".
    pub finished_at: String,
    pub duration_sec: Option<u64>,
    /// "ok" / "fail".
    pub status: String,
    /// Сколько событий журнала спровоцировало выгрузку (для watch). None для CLI.
    pub events: Option<u64>,
    /// Краткое описание что выгружено ("base, 2 ext, 5 проц").
    pub details: Option<String>,
    /// Текст ошибки, если status="fail".
    pub error: Option<String>,
}

/// Строка журнала на чтение (для GUI). Включает id и repo.
#[derive(Debug, Clone)]
pub struct ExportLogRow {
    pub id: i64,
    pub repo: String,
    pub finished_at: String,
    pub duration_sec: Option<u64>,
    pub status: String,
    pub events: Option<u64>,
    pub details: Option<String>,
    pub error: Option<String>,
}

/// Обёртка над соединением с локальной SQLite.
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Путь к БД по умолчанию: `state.db` рядом с исполняемым файлом.
    /// Если каталог exe определить не удалось — текущий рабочий каталог.
    pub fn default_path() -> PathBuf {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join("state.db");
            }
        }
        PathBuf::from("state.db")
    }

    /// Открыть (создать при отсутствии) БД и применить схему.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!("создание каталога БД {}: {}", parent.display(), e)
                })?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| anyhow::anyhow!("открытие SQLite {}: {}", path.display(), e))?;
        // WAL — чтобы GUI мог читать лог, пока watch пишет. busy_timeout — на
        // случай короткой блокировки writer'ом.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "busy_timeout", 5000_i64).ok();
        conn.pragma_update(None, "foreign_keys", "ON").ok();
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    /// Открыть БД по умолчанию (рядом с exe).
    pub fn open_default() -> anyhow::Result<Self> {
        Self::open(&Self::default_path())
    }

    fn init_schema(&self) -> anyhow::Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS ext_hashes (
                repo TEXT NOT NULL,
                name TEXT NOT NULL,
                hash TEXT NOT NULL,
                PRIMARY KEY (repo, name)
            );

            CREATE TABLE IF NOT EXISTS proc_items (
                repo    TEXT NOT NULL,
                uuid    TEXT NOT NULL,
                name    TEXT NOT NULL,
                kind    TEXT NOT NULL,
                hash    TEXT NOT NULL,
                path    TEXT NOT NULL,
                size    INTEGER NOT NULL,
                updated TEXT NOT NULL,
                PRIMARY KEY (repo, uuid)
            );

            CREATE TABLE IF NOT EXISTS proc_discovery (
                repo           TEXT PRIMARY KEY,
                tbl            TEXT NOT NULL,
                field_storage  TEXT NOT NULL,
                field_hash     TEXT NOT NULL,
                field_kind     TEXT NOT NULL,
                enum_table     TEXT NOT NULL DEFAULT '',
                hash_is_binary INTEGER NOT NULL,
                discovered_at  TEXT,
                kind_map_json  TEXT NOT NULL DEFAULT '{}'
            );

            CREATE TABLE IF NOT EXISTS export_log (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                repo         TEXT NOT NULL,
                finished_at  TEXT NOT NULL,
                duration_sec INTEGER,
                status       TEXT NOT NULL,
                events       INTEGER,
                details      TEXT,
                error        TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_export_log_repo_time
                ON export_log(repo, finished_at DESC);
            "#,
        )?;

        // Миграция существующих БД, созданных до появления колонки enum_table:
        // CREATE TABLE IF NOT EXISTS выше не добавляет колонку в уже существующую
        // таблицу. ALTER ... ADD COLUMN идемпотентен — при повторном запуске падает
        // с "duplicate column name", эту ошибку глотаем через `let _`.
        let _ = self.conn.execute(
            "ALTER TABLE proc_discovery ADD COLUMN enum_table TEXT NOT NULL DEFAULT ''",
            [],
        );

        Ok(())
    }

    // ── Хеши расширений ──────────────────────────────────────────────────

    /// Прошлые хеши расширений данного репо (имя → hash-sum). Пусто = все
    /// расширения считаются изменившимися (полная выгрузка).
    pub fn load_extension_hashes(&self, repo: &str) -> anyhow::Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, hash FROM ext_hashes WHERE repo = ?1")?;
        let rows = stmt.query_map([repo], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (name, hash) = row?;
            map.insert(name, hash);
        }
        Ok(map)
    }

    /// Полностью заменить набор хешей расширений для репо (как старый
    /// `save_current_hashes` писал весь файл целиком). Транзакция.
    pub fn save_extension_hashes(
        &mut self,
        repo: &str,
        hashes: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM ext_hashes WHERE repo = ?1", [repo])?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO ext_hashes (repo, name, hash) VALUES (?1, ?2, ?3)")?;
            for (name, hash) in hashes {
                stmt.execute(rusqlite::params![repo, name, hash])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Хеши допобработок ────────────────────────────────────────────────

    /// Прошлые записи допобработок репо (uuid → ProcItem).
    pub fn load_processings(&self, repo: &str) -> anyhow::Result<BTreeMap<String, ProcItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, name, kind, hash, path, size, updated \
             FROM proc_items WHERE repo = ?1",
        )?;
        let rows = stmt.query_map([repo], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ProcItem {
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    hash: r.get(3)?,
                    path: r.get(4)?,
                    size: r.get::<_, i64>(5)? as u64,
                    updated: r.get(6)?,
                },
            ))
        })?;
        let mut map = BTreeMap::new();
        for row in rows {
            let (uuid, item) = row?;
            map.insert(uuid, item);
        }
        Ok(map)
    }

    /// Полностью заменить набор записей допобработок для репо. Транзакция.
    pub fn save_processings(
        &mut self,
        repo: &str,
        items: &BTreeMap<String, ProcItem>,
    ) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM proc_items WHERE repo = ?1", [repo])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO proc_items (repo, uuid, name, kind, hash, path, size, updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (uuid, item) in items {
                stmt.execute(rusqlite::params![
                    repo,
                    uuid,
                    item.name,
                    item.kind,
                    item.hash,
                    item.path,
                    item.size as i64,
                    item.updated,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Discovery-кэш допобработок ───────────────────────────────────────

    /// Кэш структуры хранения справочника ДопОбработок для репо (если есть).
    pub fn load_discovery(&self, repo: &str) -> anyhow::Result<Option<Discovery>> {
        let mut stmt = self.conn.prepare(
            "SELECT tbl, field_storage, field_hash, field_kind, hash_is_binary, \
                    discovered_at, kind_map_json, enum_table \
             FROM proc_discovery WHERE repo = ?1",
        )?;
        let mut rows = stmt.query([repo])?;
        if let Some(r) = rows.next()? {
            let kind_json: String = r.get(6)?;
            let kind_uuid_to_name: HashMap<String, String> =
                serde_json::from_str(&kind_json).unwrap_or_default();
            Ok(Some(Discovery {
                table: r.get(0)?,
                field_storage: r.get(1)?,
                field_hash: r.get(2)?,
                field_kind: r.get(3)?,
                hash_is_binary: r.get::<_, i64>(4)? != 0,
                discovered_at: r.get(5)?,
                kind_uuid_to_name,
                enum_table: r.get(7)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Записать/обновить discovery-кэш для репо (UPSERT).
    pub fn save_discovery(&mut self, repo: &str, d: &Discovery) -> anyhow::Result<()> {
        let kind_json = serde_json::to_string(&d.kind_uuid_to_name).unwrap_or_else(|_| "{}".into());
        self.conn.execute(
            "INSERT INTO proc_discovery \
               (repo, tbl, field_storage, field_hash, field_kind, hash_is_binary, discovered_at, kind_map_json, enum_table) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(repo) DO UPDATE SET \
               tbl=excluded.tbl, field_storage=excluded.field_storage, \
               field_hash=excluded.field_hash, field_kind=excluded.field_kind, \
               hash_is_binary=excluded.hash_is_binary, discovered_at=excluded.discovered_at, \
               kind_map_json=excluded.kind_map_json, enum_table=excluded.enum_table",
            rusqlite::params![
                repo,
                d.table,
                d.field_storage,
                d.field_hash,
                d.field_kind,
                d.hash_is_binary as i64,
                d.discovered_at,
                kind_json,
                d.enum_table,
            ],
        )?;
        Ok(())
    }

    // ── История выгрузок ─────────────────────────────────────────────────

    /// Добавить запись о выгрузке (append-only).
    pub fn log_export(&mut self, e: &ExportLogEntry) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO export_log \
               (repo, finished_at, duration_sec, status, events, details, error) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                e.repo,
                e.finished_at,
                e.duration_sec.map(|v| v as i64),
                e.status,
                e.events.map(|v| v as i64),
                e.details,
                e.error,
            ],
        )?;
        Ok(())
    }

    /// Прочитать последние записи журнала (новые сверху). `repo=None` — по всем
    /// базам, иначе только указанная.
    pub fn read_export_log(
        &self,
        repo: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<ExportLogRow>> {
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<ExportLogRow> {
            Ok(ExportLogRow {
                id: r.get(0)?,
                repo: r.get(1)?,
                finished_at: r.get(2)?,
                duration_sec: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                status: r.get(4)?,
                events: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                details: r.get(6)?,
                error: r.get(7)?,
            })
        };
        let mut out = Vec::new();
        match repo {
            Some(repo) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, repo, finished_at, duration_sec, status, events, details, error \
                     FROM export_log WHERE repo = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![repo, limit as i64], map_row)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, repo, finished_at, duration_sec, status, events, details, error \
                     FROM export_log ORDER BY id DESC LIMIT ?1",
                )?;
                let rows = stmt.query_map(rusqlite::params![limit as i64], map_row)?;
                for row in rows {
                    out.push(row?);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (StateDb, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = StateDb::open(&dir.path().join("state.db")).unwrap();
        (db, dir)
    }

    #[test]
    fn ext_hashes_roundtrip_and_isolation_by_repo() {
        let (mut db, _g) = temp_db();
        let mut a = HashMap::new();
        a.insert("ДоработкаУТ".to_string(), "h1".to_string());
        a.insert("MCP".to_string(), "h2".to_string());
        db.save_extension_hashes("ut", &a).unwrap();

        let mut b = HashMap::new();
        b.insert("ДоработкаБП".to_string(), "h3".to_string());
        db.save_extension_hashes("bp", &b).unwrap();

        // Репо изолированы: ut не видит хеши bp.
        let got_ut = db.load_extension_hashes("ut").unwrap();
        assert_eq!(got_ut.len(), 2);
        assert_eq!(got_ut.get("MCP").map(String::as_str), Some("h2"));
        let got_bp = db.load_extension_hashes("bp").unwrap();
        assert_eq!(got_bp.len(), 1);
        assert_eq!(got_bp.get("ДоработкаБП").map(String::as_str), Some("h3"));
        // Несуществующее репо — пусто.
        assert!(db.load_extension_hashes("zup").unwrap().is_empty());
    }

    #[test]
    fn ext_hashes_save_replaces_whole_set() {
        let (mut db, _g) = temp_db();
        let mut v1 = HashMap::new();
        v1.insert("A".to_string(), "1".to_string());
        v1.insert("B".to_string(), "2".to_string());
        db.save_extension_hashes("ut", &v1).unwrap();
        // Новый набор без B — B должен исчезнуть (полная замена).
        let mut v2 = HashMap::new();
        v2.insert("A".to_string(), "10".to_string());
        db.save_extension_hashes("ut", &v2).unwrap();
        let got = db.load_extension_hashes("ut").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got.get("A").map(String::as_str), Some("10"));
        assert!(!got.contains_key("B"));
    }

    #[test]
    fn processings_roundtrip() {
        let (mut db, _g) = temp_db();
        let mut items = BTreeMap::new();
        items.insert(
            "UUID1".to_string(),
            ProcItem {
                name: "Отчёт продаж".into(),
                kind: "report".into(),
                hash: "ABC".into(),
                path: "processings/Отчёт продаж.erf".into(),
                size: 12345,
                updated: "2026-06-02T14:00:00+0300".into(),
            },
        );
        db.save_processings("ut", &items).unwrap();
        let got = db.load_processings("ut").unwrap();
        assert_eq!(got.len(), 1);
        let it = got.get("UUID1").unwrap();
        assert_eq!(it.name, "Отчёт продаж");
        assert_eq!(it.size, 12345);
        assert_eq!(it.hash, "ABC");
    }

    #[test]
    fn discovery_upsert() {
        let (mut db, _g) = temp_db();
        assert!(db.load_discovery("ut").unwrap().is_none());
        let mut kinds = HashMap::new();
        kinds.insert("AABB".to_string(), "ДополнительныйОтчёт".to_string());
        let d = Discovery {
            table: "_Reference181".into(),
            field_storage: "_Fld4776".into(),
            field_hash: "_Version".into(),
            field_kind: "_Fld4766RRef".into(),
            enum_table: "_Enum1315".into(),
            hash_is_binary: true,
            discovered_at: Some("2026-06-02T14:00:00Z".into()),
            kind_uuid_to_name: kinds,
        };
        db.save_discovery("ut", &d).unwrap();
        let got = db.load_discovery("ut").unwrap().unwrap();
        assert_eq!(got, d);
        // UPSERT: повторная запись обновляет, не дублирует.
        let mut d2 = d.clone();
        d2.field_hash = "_Fld9999".into();
        db.save_discovery("ut", &d2).unwrap();
        let got2 = db.load_discovery("ut").unwrap().unwrap();
        assert_eq!(got2.field_hash, "_Fld9999");
    }

    #[test]
    fn export_log_append_and_read_order() {
        let (mut db, _g) = temp_db();
        for i in 0..3 {
            db.log_export(&ExportLogEntry {
                repo: "ut".into(),
                finished_at: format!("2026-06-02 14:0{}:00", i),
                duration_sec: Some(40 + i),
                status: "ok".into(),
                events: Some(i),
                details: Some(format!("выгрузка {}", i)),
                error: None,
            })
            .unwrap();
        }
        db.log_export(&ExportLogEntry {
            repo: "bp".into(),
            finished_at: "2026-06-02 15:00:00".into(),
            duration_sec: None,
            status: "fail".into(),
            events: None,
            details: None,
            error: Some("git push упал".into()),
        })
        .unwrap();

        // Все базы, новые сверху.
        let all = db.read_export_log(None, 10).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].repo, "bp");
        assert_eq!(all[0].status, "fail");
        assert_eq!(all[0].error.as_deref(), Some("git push упал"));

        // Фильтр по репо.
        let ut = db.read_export_log(Some("ut"), 10).unwrap();
        assert_eq!(ut.len(), 3);
        assert!(ut.iter().all(|r| r.repo == "ut"));
        // limit работает.
        assert_eq!(db.read_export_log(Some("ut"), 2).unwrap().len(), 2);
    }
}
