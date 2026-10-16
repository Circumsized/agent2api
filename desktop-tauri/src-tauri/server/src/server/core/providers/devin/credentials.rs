//! Devin 凭证：一枚会话令牌（`devin-session-token$<jwt>`）+ 可选的服务器覆盖。
//!
//! ── 这一家的凭证形态与别家不同在哪 ────────────────────────────
//! 没有 access/refresh 两段：会话令牌本身就是长期凭据，**上游没有续期接口**
//! （`supports_refresh` 恒为 false）。令牌被 Devin 拒绝时只有一个处置 —— 重新登录
//! （参考实现：`unauthenticated` → 401 →「sign in to Devin again」）。
//!
//! ── 来源一：本机 devin CLI 的 `credentials.toml`（**只读导入**）────
//! CLI 把登录态写在这份 TOML 里（`windsurf_api_key` / `api_server_url` /
//! `devin_webapp_host` / `devin_api_url`）。导入路径与 CLI 完全一致：
//!   - Windows：`%APPDATA%\devin\credentials.toml`
//!   - 其余：`$XDG_DATA_HOME/devin/credentials.toml`，缺省 `~/.local/share/devin/…`
//! 我们**只读、绝不写**（CLI 自己的登录态不属于网关），也不去改它的配置。
//! 参考实现为了跑 `devin models list` 会另起一个属于插件自己的数据目录 ——
//! 本仓不做那件事（目录清单由 `GetCliModelConfigs` 直接取，见 `models.rs`），
//! 所以这里只有读。
//!
//! ── 来源二：手工粘贴令牌 ─────────────────────────────────────
//! 与别家「粘贴凭证」同一入口，字段名兼容几种写法（见 [`Credential::from_payload`]）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::endpoints;

/// 一份 Devin 凭证。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Credential {
    /// 会话令牌（`devin-session-token$<jwt>`），转发鉴权头的全部内容
    #[serde(default)]
    pub token: String,
    /// 应用服务器基址覆盖（空 = 用 [`endpoints::SERVER`]）
    #[serde(default)]
    pub api_server_url: String,
    /// webapp 主机（仅登录时回显，转发不用）
    #[serde(default)]
    pub webapp_host: String,
    /// Devin 的 REST API 基址（登录时回显，转发不用 —— 那是 CLI /webapp 那条平面）
    #[serde(default)]
    pub api_url: String,
    /// 账号邮箱 / 标识（能拿到时填，用于去重与展示；判不出则空）
    #[serde(default)]
    pub email: String,
}

impl Credential {
    /// 能拿它发请求吗（令牌非空即可）。
    pub fn valid(&self) -> bool {
        !self.token.trim().is_empty()
    }

    /// 本次转发要用的应用服务器基址（含按账号覆盖）。
    pub fn server(&self) -> String {
        endpoints::server_base(Some(&self.api_server_url))
    }

    /// 去重 / 单飞用的身份：优先邮箱，没有就退到令牌（整串，够唯一）。
    pub fn identity(&self) -> String {
        let email = self.email.trim();
        if !email.is_empty() {
            return email.to_string();
        }
        self.token.trim().to_string()
    }

    /// 从**粘贴的 payload** 读一份凭证（兼容几种字段名）。
    ///
    /// 认得的写法（按优先级）：`token` / `accessToken` / `apiKey` / `sessionToken` /
    /// `session_token` / `key`。服务器覆盖认 `api_server_url` / `apiServerUrl` /
    /// `server` / `serverUrl`。整段 `credentials.toml` 文本也可以（`toml` 字段，
    /// 或直接把 TOML 贴在 `token` 上——见下）。
    pub fn from_payload(payload: &Value) -> Result<Self, String> {
        let text_of = |keys: &[&str]| -> String {
            keys.iter()
                .find_map(|key| payload.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .unwrap_or("")
                .to_string()
        };
        let mut credential = Credential {
            token: text_of(&[
                "token",
                "accessToken",
                "apiKey",
                "sessionToken",
                "session_token",
                "key",
            ]),
            api_server_url: text_of(&["api_server_url", "apiServerUrl", "server", "serverUrl"]),
            webapp_host: text_of(&["devin_webapp_host", "devinWebappHost", "webappHost"]),
            api_url: text_of(&["devin_api_url", "devinApiUrl", "apiUrl"]),
            email: text_of(&["email", "userId", "user"]),
        };
        // 用户可能直接把整段 credentials.toml 贴进输入框 —— 那样 token 字段会拿到
        // 一整段文本。按 TOML 解一次，能解出 windsurf_api_key 就用它（这不是
        // 「顺手帮忙」：粘贴 TOML 是这类网关常见的使用方式，不认会让用户以为
        // 「我的 key 明明是对的」）。
        let looks_like_toml = credential.token.contains('=') || credential.token.contains('\n');
        if looks_like_toml {
            if let Some(key) = parse_credentials_toml(&credential.token).get("windsurf_api_key") {
                let parsed = parse_credentials_toml(&credential.token);
                credential.token = key.clone();
                credential.api_server_url = parsed
                    .get("api_server_url")
                    .cloned()
                    .unwrap_or(credential.api_server_url);
                credential.webapp_host = parsed
                    .get("devin_webapp_host")
                    .cloned()
                    .unwrap_or(credential.webapp_host);
                credential.api_url = parsed
                    .get("devin_api_url")
                    .cloned()
                    .unwrap_or(credential.api_url);
            }
        }
        if !credential.valid() {
            return Err("Devin 凭据缺少会话令牌（token / accessToken）".to_string());
        }
        Ok(credential)
    }

    /// 落库形状：本家专有字段之外**同时写通用键名**（`accessToken` / `refreshToken` /
    /// `userId` / `expiresAt`）。
    ///
    /// 理由与 CodeArts / Qoder 逐字相同（见 `codearts_accounts.rs` 的模块头）：
    /// 账号存储的投影（`StoredAccount::has_token` → `access_token()`）读的是
    /// `accessToken`，不写它，Devin 账号会在选路时被当成「没有凭证」而永远选不中。
    /// `refreshToken` 恒为空 —— 本家**没有续期**，写一个空串让「有没有 refresh」
    /// 这条通用判定如实为假（而不是缺键，缺键在展示侧与空串等价但更易被误读）。
    pub fn to_record_value(&self) -> Value {
        let mut record = match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        record.insert("accessToken".to_string(), Value::String(self.token.clone()));
        record.insert("refreshToken".to_string(), Value::String(String::new()));
        record.insert("userId".to_string(), Value::String(self.email.clone()));
        // 没有过期时间可言（会话令牌的有效期由上游控制），恒 null
        record.insert("expiresAt".to_string(), Value::Null);
        Value::Object(record)
    }
}

/// 解析一份 devin CLI 的 `credentials.toml`（**极简 TOML**：只认顶层的 `key = "值"`）。
///
/// 为什么手写而不上 `toml` crate：CLI 写出来的这份文件是固定四行的简单键值，
/// 任何一行不认就丢掉那一行即可；用完整 TOML 解析器反而会把「语法有误」整份拒掉，
/// 而我们要的是「尽力读出 windsurf_api_key」。参考实现的 `readCredentials` 就是
/// 这套正则：`^\s*([A-Za-z0-9_]+)\s*=\s*("…"|'…')\s*$`。
///
/// 双引号的值按 CLI 的写法处理转义（`\\` → `\`，`\"` → `"`）；单引号是字面量，
/// 原样保留。
pub fn parse_credentials_toml(text: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            continue;
        }
        let mut value = value.trim();
        // 行尾注释（只对无引号 / 单引号值有意义，但先剥掉再看开头字符更省分支）
        if let Some(rest) = value.strip_prefix('"') {
            // 双引号：读到未转义的收尾引号
            let mut buf = String::new();
            let mut chars = rest.chars();
            let mut closed = false;
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        buf.push(next);
                    }
                    continue;
                }
                if ch == '"' {
                    closed = true;
                    break;
                }
                buf.push(ch);
            }
            if closed {
                out.insert(key.to_string(), buf);
            }
        } else if let Some(rest) = value.strip_prefix('\'') {
            if let Some(end) = rest.find('\'') {
                out.insert(key.to_string(), rest[..end].to_string());
            }
        } else {
            // 无引号的值：取到行尾（罕见，但某些工具会这么写）
            if let Some(end) = value.find(|ch: char| ch == '#' || ch == '\r') {
                value = value[..end].trim();
            }
            if !value.is_empty() {
                out.insert(key.to_string(), value.to_string());
            }
        }
    }
    out
}

/// 本机 devin CLI 的登录态文件路径（`None` = 判不出家目录）。
///
/// 与参考实现的 `cliCredentialsPath()` 逐字一致：
/// Windows 用 `%APPDATA%\devin\credentials.toml`；其余用
/// `$XDG_DATA_HOME/devin/credentials.toml`，`XDG_DATA_HOME` 未设时回落到
/// `~/.local/share/devin/credentials.toml`。
pub fn cli_credentials_path() -> Option<std::path::PathBuf> {
    if cfg!(target_os = "windows") {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let appdata = appdata.trim();
            if !appdata.is_empty() {
                return Some(std::path::PathBuf::from(appdata).join("devin").join("credentials.toml"));
            }
        }
    }
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".local").join("share"))
        })?;
    Some(base.join("devin").join("credentials.toml"))
}

/// 读本机 CLI 的登录态并转成本家凭证（**只读**；文件不存在 / 没有 key 都是错误）。
pub fn read_cli_credentials() -> Result<Credential, String> {
    let Some(path) = cli_credentials_path() else {
        return Err("读不到家目录，无法定位 devin CLI 的 credentials.toml".to_string());
    };
    let text = std::fs::read_to_string(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "Devin 在本机没有登录态：{} 不存在（请先运行 `devin auth login`）",
                path.display()
            )
        } else {
            format!("读取 devin CLI 登录态失败（{}）：{error}", path.display())
        }
    })?;
    let parsed = parse_credentials_toml(&text);
    let token = parsed
        .get("windsurf_api_key")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{} 里没有 windsurf_api_key（该 CLI 未登录）", path.display()))?;
    Ok(Credential {
        token,
        api_server_url: parsed.get("api_server_url").cloned().unwrap_or_default(),
        webapp_host: parsed.get("devin_webapp_host").cloned().unwrap_or_default(),
        api_url: parsed.get("devin_api_url").cloned().unwrap_or_default(),
        email: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn toml_reader_handles_the_cli_shape_and_escapes() {
        let text = "# devin CLI login\nwindsurf_api_key = \"devin-session-token$jwt\"\napi_server_url = 'https://server.codeium.com'\ndevin_webapp_host = \"app.devin.ai\"\ndevin_api_url = \"https://api.devin.ai\"\n";
        let parsed = parse_credentials_toml(text);
        assert_eq!("devin-session-token$jwt", parsed["windsurf_api_key"]);
        assert_eq!("https://server.codeium.com", parsed["api_server_url"]);
        assert_eq!("app.devin.ai", parsed["devin_webapp_host"]);
        // 注释行与表头行不参与，认不出的行丢掉而非整份作废
        assert!(!parsed.contains_key("# devin CLI login"));
    }

    #[test]
    fn escaped_quotes_survive_the_round_trip() {
        // CLI 写 TOML 时会把 " 转义（参考实现的 tomlString）；读侧要还原
        let text = "windsurf_api_key = \"a\\\"b\\\\c\"\n";
        let parsed = parse_credentials_toml(text);
        assert_eq!("a\"b\\c", parsed["windsurf_api_key"]);
    }

    #[test]
    fn payload_paste_accepts_several_field_names() {
        for key in ["token", "accessToken", "apiKey", "sessionToken", "key"] {
            let mut payload = serde_json::Map::new();
            payload.insert(key.to_string(), json!("devin-session-token$x"));
            let credential =
                Credential::from_payload(&Value::Object(payload)).expect("应当能解析");
            assert_eq!("devin-session-token$x", credential.token, "字段 {key} 应当被认");
        }
        assert!(Credential::from_payload(&json!({})).is_err(), "没有令牌要明确报错");
    }

    #[test]
    fn pasting_a_whole_toml_block_is_read_as_a_credential() {
        let payload = json!({
            "token": "windsurf_api_key = \"devin-session-token$z\"\napi_server_url = \"https://server.codeium.com\"\n"
        });
        let credential = Credential::from_payload(&payload).expect("整段 TOML 也应当能读");
        assert_eq!("devin-session-token$z", credential.token);
    }

    #[test]
    fn record_value_carries_the_generic_keys_the_store_reads() {
        let credential = Credential {
            token: "devin-session-token$jwt".to_string(),
            api_server_url: "https://server.codeium.com".to_string(),
            email: "me@example.invalid".to_string(),
            ..Default::default()
        };
        let record = credential.to_record_value();
        assert_eq!("devin-session-token$jwt", record["accessToken"].as_str().unwrap());
        assert_eq!("me@example.invalid", record["userId"].as_str().unwrap());
        // 没有续期：refreshToken 必须是空串（truthy 判定为假），expiresAt 恒 null
        assert_eq!("", record["refreshToken"].as_str().unwrap());
        assert!(record["expiresAt"].is_null());
        assert_eq!("https://server.codeium.com", credential.server());
        assert_eq!("me@example.invalid", credential.identity());
    }
}
