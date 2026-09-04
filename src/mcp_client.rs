//! Клиент к HTTP-сервису MCP внутри самой 1С (вариант C: per-base direct).
//!
//! Особенности:
//! - URL вида `http://<host>/<base-alias>/hs/mcp` — каждый клиент привязан к ОДНОЙ базе.
//! - Аутентификация двойная: Basic Auth (логин/пароль 1С) + заголовок `X-MCP-Key`
//!   (значение ключа, заданное в HTTP-сервисе MCP базы; в `AppConfig.mcp_api_key`).
//! - Эндпоинт JSON-RPC: `POST <url>/rpc`. Без streamable-HTTP, без session_id,
//!   без `notifications/initialized` — простой JSON-RPC.
//! - Для вызова MCP-tool (`eventlog_query`, `db_table_fields` и т.д.)
//!   шлём метод `tools/call` с `{name, arguments}` и парсим `result.content[0].text`.

use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct McpClient {
    inner: Arc<Inner>,
}

struct Inner {
    rpc_url: String,
    auth_header: String, // "Basic <base64(user:pwd)>"
    api_key: String,
    http: reqwest::Client,
}

impl McpClient {
    /// Создать клиента к MCP-сервису одной базы 1С.
    /// `base_url` — без `/rpc` на конце, например `http://localhost/demo-ut/hs/mcp`.
    pub fn new(
        base_url: impl Into<String>,
        username: impl AsRef<str>,
        password: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let base = base_url.into();
        let base = base.trim_end_matches('/').to_string();
        let rpc_url = format!("{}/rpc", base);

        let creds = format!("{}:{}", username.as_ref(), password.as_ref());
        let auth_header = format!("Basic {}", BASE64.encode(creds.as_bytes()));

        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .build()
            .map_err(|e| anyhow::anyhow!("reqwest::Client build: {}", e))?;

        Ok(Self {
            inner: Arc::new(Inner {
                rpc_url,
                auth_header,
                api_key: api_key.into(),
                http,
            }),
        })
    }

    /// Вызвать MCP tool (`tools/call` JSON-RPC). Возвращает `result.content[0].text`.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<String> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        });

        let resp = self
            .inner
            .http
            .post(&self.inner.rpc_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", &self.inner.auth_header)
            .header("X-MCP-Key", &self.inner.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MCP tools/call '{}' запрос упал: {}", name, e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("MCP tools/call '{}' вернул HTTP {}: {}", name, status, body);
        }

        let raw = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("MCP tools/call '{}' чтение тела: {}", name, e))?;
        let body: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "MCP tools/call '{}' не JSON: {}\nraw[:500]={}",
                name,
                e,
                &raw.chars().take(500).collect::<String>()
            )
        })?;

        if let Some(err) = body.get("error") {
            anyhow::bail!("MCP tools/call '{}' JSON-RPC error: {}", name, err);
        }

        let text = body
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "MCP tools/call '{}' нет result.content[0].text. body={}",
                    name,
                    serde_json::to_string(&body).unwrap_or_default()
                )
            })?
            .to_string();

        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_url_trims_trailing_slash() {
        let c = McpClient::new("http://h/base/hs/mcp/", "u", "p", "k").unwrap();
        assert_eq!(c.inner.rpc_url, "http://h/base/hs/mcp/rpc");
        let c = McpClient::new("http://h/base/hs/mcp", "u", "p", "k").unwrap();
        assert_eq!(c.inner.rpc_url, "http://h/base/hs/mcp/rpc");
    }

    #[test]
    fn basic_auth_header_format() {
        let c = McpClient::new("http://h/x/hs/mcp", "user", "pwd", "k").unwrap();
        // base64("user:pwd") = dXNlcjpwd2Q=
        assert_eq!(c.inner.auth_header, "Basic dXNlcjpwd2Q=");
    }
}
