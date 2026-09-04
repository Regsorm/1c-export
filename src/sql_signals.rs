//! Отпечатки состояния базы по служебным таблицам MS SQL — режим `changeDetection = sql`.
//!
//! Watch снимает три отпечатка и сравнивает их с сохранёнными в `state/<alias>.json`:
//!   - основная конфигурация: `MAX(Modified)` и число строк таблицы `Config`;
//!   - расширения: свёртка всего списка `_ExtensionsInfo` (не максимум по времени —
//!     удаление расширения откатывает максимум назад, а список меняется всегда);
//!   - допобработки: свёртка пар «ссылка → поле-маркер изменения» таблицы справочника.
//!
//! HTTP-сервис внутри базы для этого не нужен — хватает того же доступа к СУБД,
//! по которому и так читается справочник обработок.

use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::processings::TiberiusClient;
use crate::state::{SqlSignals, StoredMapping};

/// Имена, нужные для отпечатка допобработок: таблица справочника и поле-маркер изменения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMappingLite {
    pub table: String,
    pub field_hash: String,
    /// true — поле бинарное (rowversion/ВерсияДанных), false — строковое (КонтрольнаяСумма).
    pub hash_is_binary: bool,
}

impl StoredMappingLite {
    /// Из кеша структуры хранения в state. Признак бинарности — по имени поля,
    /// так же, как при подготовке выгрузки допобработок.
    pub fn from_stored(m: &StoredMapping) -> Self {
        Self {
            table: m.table.clone(),
            field_hash: m.field_hash.clone(),
            hash_is_binary: m.field_hash.to_lowercase().contains("version"),
        }
    }
}

/// Что именно опрашивать: только то, что база реально выгружает.
pub struct SignalScope {
    pub base: bool,
    pub extensions: bool,
    /// None — допобработки не выгружаются либо структура хранения ещё не известна.
    pub processings: Option<StoredMappingLite>,
}

/// SHA-256 (hex) от строк, соединённых переводом строки.
fn fingerprint(lines: &[String]) -> String {
    let mut h = Sha256::new();
    h.update(lines.join("\n").as_bytes());
    format!("{:x}", h.finalize())
}

/// Выражение для поля-маркера изменения — то же, что в лёгком diff-запросе выгрузки.
fn hash_select(field_hash: &str, hash_is_binary: bool) -> String {
    if hash_is_binary {
        format!("CONVERT(VARCHAR(130), CAST({} AS VARBINARY(64)), 2)", field_hash)
    } else {
        format!("RTRIM(CONVERT(NVARCHAR(64), {}))", field_hash)
    }
}

/// Отпечаток основной конфигурации: время последнего изменения и число строк `Config`.
/// Даты в `Config` хранятся со сдвигом на 2000 лет — берём их как строку, не толкуя.
async fn take_config(client: &mut TiberiusClient) -> anyhow::Result<String> {
    let sql = "SELECT CONVERT(VARCHAR(30), MAX(Modified), 121) AS m, COUNT(*) AS c \
               FROM dbo.Config WITH (NOLOCK)";
    let mut stream = client.simple_query(sql).await?;
    let mut out = String::new();
    while let Some(item) = stream.next().await {
        if let tiberius::QueryItem::Row(row) = item? {
            let m = row.get::<&str, _>("m").unwrap_or("");
            let c = row.get::<i32, _>("c").unwrap_or(0);
            out = format!("{}|{}", m, c);
        }
    }
    Ok(out)
}

/// Отпечаток списка расширений. Пустая таблица даёт свёртку пустой строки,
/// а не пустую строку — иначе «расширений нет» не отличить от «опрос не делался».
async fn take_extensions(client: &mut TiberiusClient) -> anyhow::Result<String> {
    let sql = "SELECT _ExtName AS n, CONVERT(VARCHAR(30), _UpdateTime, 121) AS t, \
               CONVERT(BIGINT, _Version) AS v \
               FROM dbo._ExtensionsInfo WITH (NOLOCK) ORDER BY _ExtName";
    let mut stream = client.simple_query(sql).await?;
    let mut lines = Vec::new();
    while let Some(item) = stream.next().await {
        if let tiberius::QueryItem::Row(row) = item? {
            let n = row.get::<&str, _>("n").unwrap_or("");
            let t = row.get::<&str, _>("t").unwrap_or("");
            let v = row.get::<i64, _>("v").unwrap_or(0);
            lines.push(format!("{}|{}|{}", n.trim(), t, v));
        }
    }
    Ok(fingerprint(&lines))
}

/// Отпечаток допобработок: ссылка и поле-маркер изменения по каждой записи справочника.
async fn take_processings(
    client: &mut TiberiusClient,
    m: &StoredMappingLite,
) -> anyhow::Result<String> {
    // Имена таблицы и поля пришли из определения структуры хранения (проверены по
    // sys.columns) — подставляются в текст запроса так же, как в выгрузке обработок.
    let sql = format!(
        "SELECT CONVERT(CHAR(32), _IDRRef, 2) AS id, {hash} AS h \
         FROM dbo.{table} WITH (NOLOCK) \
         WHERE _Marked = 0x00 AND _Folder = 0x01 ORDER BY _IDRRef",
        hash = hash_select(&m.field_hash, m.hash_is_binary),
        table = m.table,
    );
    let mut stream = client.simple_query(sql).await?;
    let mut lines = Vec::new();
    while let Some(item) = stream.next().await {
        if let tiberius::QueryItem::Row(row) = item? {
            let id = row.get::<&str, _>("id").unwrap_or("");
            let h = row.get::<&str, _>("h").unwrap_or("");
            lines.push(format!("{}|{}", id.trim(), h.trim()));
        }
    }
    Ok(fingerprint(&lines))
}

/// Снять отпечатки по всем компонентам из `scope`.
pub async fn take_signals(
    client: &mut TiberiusClient,
    scope: &SignalScope,
) -> anyhow::Result<SqlSignals> {
    let mut out = SqlSignals {
        taken_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    if scope.base {
        out.config = take_config(client).await?;
    }
    if scope.extensions {
        out.extensions = take_extensions(client).await?;
    }
    if let Some(m) = &scope.processings {
        out.processings = take_processings(client, m).await?;
    }
    Ok(out)
}

/// Человекочитаемые причины выгрузки. Пустой список — изменений нет.
pub fn diff_signals(
    prev: Option<&SqlSignals>,
    cur: &SqlSignals,
    scope: &SignalScope,
) -> Vec<String> {
    let Some(prev) = prev else {
        return vec!["первый опрос служебных таблиц — выгружаем полностью".to_string()];
    };
    let mut reasons = Vec::new();
    if scope.base && prev.config != cur.config {
        reasons.push(format!(
            "основная конфигурация: Config было '{}' стало '{}'",
            prev.config, cur.config
        ));
    }
    if scope.extensions && prev.extensions != cur.extensions {
        reasons.push("расширения: отпечаток _ExtensionsInfo изменился".to_string());
    }
    if scope.processings.is_some() && prev.processings != cur.processings {
        reasons.push("допобработки: отпечаток изменился".to_string());
    }
    reasons
}

/// Сдвинулся ли отпечаток основной конфигурации — признак для снимка `base.cf`.
/// Первый опрос (прошлых отпечатков нет) считаем изменением: снимок тогда нужен.
/// База не выгружает основную конфигурацию — изменений по ней нет.
pub fn config_changed(prev: Option<&SqlSignals>, cur: &SqlSignals, scope: &SignalScope) -> bool {
    if !scope.base {
        return false;
    }
    match prev {
        None => true,
        Some(prev) => prev.config != cur.config,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_all() -> SignalScope {
        SignalScope {
            base: true,
            extensions: true,
            processings: Some(StoredMappingLite {
                table: "_Reference181".into(),
                field_hash: "_Version".into(),
                hash_is_binary: true,
            }),
        }
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = vec!["ext1|2026-09-04|1".to_string(), "ext2|2026-09-04|2".to_string()];
        let b = a.clone();
        assert_eq!(fingerprint(&a), fingerprint(&b));

        // Изменение одной строки меняет отпечаток.
        let mut c = a.clone();
        c[1] = "ext2|2026-09-04|3".to_string();
        assert_ne!(fingerprint(&a), fingerprint(&c));

        // Удаление строки тоже.
        assert_ne!(fingerprint(&a), fingerprint(&a[..1].to_vec()));

        // Пустой список даёт не пустую строку, а свёртку пустого текста.
        assert_eq!(fingerprint(&[]).len(), 64);
    }

    #[test]
    fn hash_select_binary_and_string() {
        assert_eq!(
            hash_select("_Version", true),
            "CONVERT(VARCHAR(130), CAST(_Version AS VARBINARY(64)), 2)"
        );
        assert_eq!(
            hash_select("_Fld4776", false),
            "RTRIM(CONVERT(NVARCHAR(64), _Fld4776))"
        );
    }

    #[test]
    fn binary_flag_by_field_name() {
        let m = StoredMapping {
            table: "_Reference181".into(),
            field_storage: "_Fld4776".into(),
            field_hash: "_Version".into(),
            field_kind: "_Fld4766RRef".into(),
            enum_table: "_Enum1315".into(),
            fetched_at: "2026-09-04T10:00:00+00:00".into(),
        };
        assert!(StoredMappingLite::from_stored(&m).hash_is_binary);

        let m2 = StoredMapping { field_hash: "_Fld4777".into(), ..m };
        assert!(!StoredMappingLite::from_stored(&m2).hash_is_binary);
    }

    #[test]
    fn diff_first_run_is_a_reason() {
        let cur = SqlSignals::default();
        let reasons = diff_signals(None, &cur, &scope_all());
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("первый опрос"));
    }

    #[test]
    fn diff_no_changes_is_empty() {
        let cur = SqlSignals {
            config: "2026-09-04|10".into(),
            extensions: "aa".into(),
            processings: "bb".into(),
            taken_at: "t2".into(),
        };
        let prev = SqlSignals { taken_at: "t1".into(), ..cur.clone() };
        // Время снятия отпечатков на сравнение не влияет.
        assert!(diff_signals(Some(&prev), &cur, &scope_all()).is_empty());
    }

    #[test]
    fn diff_reports_each_component() {
        let prev = SqlSignals {
            config: "c1".into(),
            extensions: "e1".into(),
            processings: "p1".into(),
            taken_at: String::new(),
        };
        let cur = SqlSignals {
            config: "c2".into(),
            extensions: "e2".into(),
            processings: "p2".into(),
            taken_at: String::new(),
        };
        let reasons = diff_signals(Some(&prev), &cur, &scope_all());
        assert_eq!(reasons.len(), 3);
        assert!(reasons[0].contains("основная конфигурация"));
        assert!(reasons[1].contains("расширения"));
        assert!(reasons[2].contains("допобработки"));
    }

    #[test]
    fn diff_ignores_components_outside_scope() {
        let prev = SqlSignals {
            config: "c1".into(),
            extensions: "e1".into(),
            processings: "p1".into(),
            taken_at: String::new(),
        };
        let cur = SqlSignals {
            config: "c2".into(),
            extensions: "e2".into(),
            processings: "p2".into(),
            taken_at: String::new(),
        };
        // База выгружает только основную конфигурацию — остальные различия не считаются.
        let scope = SignalScope { base: true, extensions: false, processings: None };
        let reasons = diff_signals(Some(&prev), &cur, &scope);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("основная конфигурация"));
    }

    #[test]
    fn config_changed_by_config_fingerprint_only() {
        let prev = SqlSignals {
            config: "c1".into(),
            extensions: "e1".into(),
            processings: "p1".into(),
            taken_at: String::new(),
        };
        // Изменились только расширения — основная конфигурация не менялась.
        let only_ext = SqlSignals { extensions: "e2".into(), ..prev.clone() };
        assert!(!config_changed(Some(&prev), &only_ext, &scope_all()));

        // Сдвинулся отпечаток Config — менялась.
        let cfg_moved = SqlSignals { config: "c2".into(), ..prev.clone() };
        assert!(config_changed(Some(&prev), &cfg_moved, &scope_all()));

        // Первый опрос — считаем изменением.
        assert!(config_changed(None, &prev, &scope_all()));

        // Основная конфигурация не выгружается — признака нет.
        let scope = SignalScope { base: false, extensions: true, processings: None };
        assert!(!config_changed(None, &prev, &scope));
    }
}
