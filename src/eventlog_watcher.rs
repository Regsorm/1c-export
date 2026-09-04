//! Опрос журнала регистрации 1С через MCP `eventlog_query` с гибридной
//! дедупликацией.
//!
//! Особенности журнала 1С:
//! - В ответе НЕТ RowID или порядкового номера события — только `Дата` с
//!   точностью до секунды.
//! - Параметр `from` MCP-вызова — **включающий**: вернутся события с timestamp
//!   `>= from`.
//! - В одну секунду может произойти несколько событий — нужно различать.
//!
//! Дедупликация:
//! - В state хранится `last_processed_at` (max timestamp обработанных событий)
//!   и `processed_hashes_at_last_dt` (SHA-256 событий ровно с этим timestamp).
//! - Запрос: `from = last_processed_at`. Получаем события >= last_dt.
//! - Локально фильтруем: исключаем события с `Дата == last_dt` и хэшем из
//!   `processed_hashes_at_last_dt`. Это гарантирует:
//!     * ни одно событие не теряется (from включающий);
//!     * ни одно не обрабатывается дважды (хэш-фильтр для коллизий внутри секунды).
//! - После успешной обработки сдвигаем курсор: `last_processed_at = max(events.Дата)`,
//!   `processed_hashes_at_last_dt = [хэши событий ровно с max-timestamp]`.

use std::collections::HashSet;

use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::bases_config::{BaseEntry, DaemonConfig};
use crate::mcp_client::McpClient;
use crate::state::BaseState;

/// Одна запись из журнала регистрации в формате, который отдаёт HTTP-сервис MCP базы.
/// Имена полей русские (как в ответе) — десериализуем напрямую через serde rename.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LogEvent {
    #[serde(rename = "Дата")]
    pub date: String,           // "2026-04-26T21:02:38"
    #[serde(rename = "Событие")]
    pub event: String,          // "_$InfoBase$_.DBConfigUpdate"
    #[serde(rename = "ИмяПользователя", default)]
    pub user: String,
    #[serde(rename = "Метаданные", default)]
    pub metadata: String,
    #[serde(rename = "Комментарий", default)]
    pub comment: String,
    #[serde(rename = "ПредставлениеДанных", default)]
    pub data_pres: String,
    #[serde(rename = "Уровень", default)]
    pub level: String,
}

impl LogEvent {
    /// Стабильный SHA-256 от ключевых полей события — отпечаток для де-дапа.
    pub fn hash(&self) -> String {
        // Стабильный JSON: serde_json::to_vec на структуре с фиксированным порядком полей.
        // Используем явный JSON-объект с нужными полями (без `level`/опциональных
        // — они не влияют на уникальность, но в ответе всегда есть).
        let v = json!({
            "Дата": self.date,
            "Событие": self.event,
            "ИмяПользователя": self.user,
            "Метаданные": self.metadata,
            "Комментарий": self.comment,
            "ПредставлениеДанных": self.data_pres,
        });
        let bytes = serde_json::to_vec(&v).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(&bytes);
        format!("{:x}", h.finalize())
    }
}

/// Опрашивает MCP eventlog_query, возвращает только новые события (после де-дапа).
/// Сортировку по Дате делает локально на всякий случай (MCP вроде упорядочивает,
/// но явно не гарантирует).
pub async fn query_new_events(
    mcp: &McpClient,
    base: &BaseEntry,
    cfg: &DaemonConfig,
    state: &BaseState,
) -> anyhow::Result<Vec<LogEvent>> {
    // 1. from — либо last_processed_at, либо «N часов назад» при первом запуске.
    let from_dt = match &state.last_processed_at {
        Some(s) => s.clone(),
        None => {
            let from = Utc::now() - ChronoDuration::hours(cfg.lookback_hours_first_run as i64);
            from.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
    };

    // 2. MCP запрос. columns намеренно не передаём — у сервиса там ошибка
    //    JSON-Schema валидации, используем набор колонок по умолчанию.
    // URL клиента уже привязан к одной базе (вариант C: per-base direct), параметр
    // `base=` в payload не нужен — он использовался только для маршрутизации через общий роутер.
    let _ = base; // подавляем warning unused, оставляем сигнатуру совместимой
    let resp_text = mcp
        .call_tool(
            "eventlog_query",
            json!({
                "from": from_dt,
                "limit": 500,
                "filter": { "events": cfg.trigger_events.clone() }
            }),
        )
        .await?;

    // 3. Парсим: ожидаем { "success": true, "items": [...], "meta": {...} }
    let parsed: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| anyhow::anyhow!("eventlog_query возврат не JSON: {}\nbody[:500]={}", e, &resp_text.chars().take(500).collect::<String>()))?;
    let items = parsed
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut events: Vec<LogEvent> = items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    // 4. Локальная дедупликация: исключаем коллизии секунды last_processed_at.
    if let Some(ref last_dt) = state.last_processed_at {
        let dup_set: HashSet<&String> = state.processed_hashes_at_last_dt.iter().collect();
        events.retain(|e| !(e.date == *last_dt && dup_set.contains(&e.hash())));
    }

    // 5. Сортировка по дате на всякий случай — некоторые серверы возвращают по убыванию.
    events.sort_by(|a, b| a.date.cmp(&b.date));

    Ok(events)
}

/// После успешной обработки сдвинуть курсор state-а на максимальный timestamp
/// обработанных событий и запомнить хэши событий с этим timestamp (для следующей
/// итерации де-дапа).
pub fn mark_events_processed(state: &mut BaseState, events: &[LogEvent]) {
    if events.is_empty() {
        return;
    }
    let max_dt = events.iter().map(|e| &e.date).max().unwrap().clone();
    state.last_processed_at = Some(max_dt.clone());
    state.processed_hashes_at_last_dt = events
        .iter()
        .filter(|e| e.date == max_dt)
        .map(|e| e.hash())
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(date: &str, event: &str, user: &str) -> LogEvent {
        LogEvent {
            date: date.into(),
            event: event.into(),
            user: user.into(),
            metadata: String::new(),
            comment: String::new(),
            data_pres: String::new(),
            level: "Информация".into(),
        }
    }

    #[test]
    fn hash_stable_and_distinct() {
        let a = ev("2026-04-26T21:02:38", "_$InfoBase$_.DBConfigUpdate", "Иванов");
        let b = ev("2026-04-26T21:02:38", "_$InfoBase$_.DBConfigUpdate", "Иванов");
        let c = ev("2026-04-26T21:02:38", "_$InfoBase$_.DBConfigUpdate", "Петров");
        assert_eq!(a.hash(), b.hash(), "одинаковые события — одинаковый хэш");
        assert_ne!(a.hash(), c.hash(), "разные пользователи — разные хэши");
    }

    #[test]
    fn mark_processed_writes_max_timestamp_and_hashes() {
        let mut state = BaseState::default();
        let events = vec![
            ev("2026-04-26T21:00:00", "X", "U1"),
            ev("2026-04-26T21:02:38", "X", "U1"),
            ev("2026-04-26T21:02:38", "X", "U2"),  // тот же timestamp, другой пользователь
            ev("2026-04-26T21:02:38", "Y", "U1"),  // тот же timestamp, другое событие
        ];
        mark_events_processed(&mut state, &events);
        assert_eq!(state.last_processed_at.as_deref(), Some("2026-04-26T21:02:38"));
        assert_eq!(state.processed_hashes_at_last_dt.len(), 3,
            "три события ровно с max-timestamp должны попасть в дедуп-набор");
        // Хэш события с timestamp 21:00:00 не должен оказаться в наборе
        let h_old = ev("2026-04-26T21:00:00", "X", "U1").hash();
        assert!(!state.processed_hashes_at_last_dt.contains(&h_old));
    }

    #[test]
    fn mark_processed_noop_on_empty() {
        let mut state = BaseState {
            last_processed_at: Some("2026-04-26T20:00:00".into()),
            processed_hashes_at_last_dt: vec!["x".into()],
            ..Default::default()
        };
        mark_events_processed(&mut state, &[]);
        // Курсор не двигается при пустом наборе
        assert_eq!(state.last_processed_at.as_deref(), Some("2026-04-26T20:00:00"));
        assert_eq!(state.processed_hashes_at_last_dt, vec!["x".to_string()]);
    }
}
