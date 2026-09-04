//! Best-effort POST на кэш-прокси для инвалидации после успешной выгрузки.
//! Если прокси нет / недоступен — это не ошибка, просто warning в лог.

use crate::logging::Logger;

pub async fn send(proxy_url: &str, base_alias: &str) {
    let url = format!("{}/flush?base={}", proxy_url.trim_end_matches('/'), base_alias);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            Logger::log(&format!("[flush] reqwest build не удался: {}", e));
            return;
        }
    };
    match client.post(&url).send().await {
        Ok(r) if r.status().is_success() => {
            Logger::log(&format!("[flush] {} ok", url));
        }
        Ok(r) => {
            Logger::log(&format!("[flush] {} вернул {}", url, r.status()));
        }
        Err(e) => {
            Logger::log(&format!("[flush] {} ошибка: {}", url, e));
        }
    }
}
