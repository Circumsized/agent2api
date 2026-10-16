//! Devin 的**网页登录**（PKCE 往返）与本机 CLI 登录态的身份确认。
//!
//! ── 这条链长什么样（`devin auth login` 那一轮，由网关自己跑）────────
//!   ① 本地生成 PKCE verifier + state，拼出授权地址
//!      `https://app.devin.ai/auth/cli/continue?redirect_uri=…&state=…&code_challenge=…&…`；
//!   ② 用户在浏览器完成授权，Devin 把浏览器 302 回
//!      `http://127.0.0.1:<port>/callback?code=…&state=…`；
//!   ③ 网关用 `code` + `code_verifier` + `redirect_uri` 调
//!      `ExchangeDevinCLIPKCECode`，换回**已经成形**的会话令牌
//!      `devin-session-token$<jwt>`（参考实现原话：CLI 自己存的就是它）。
//!
//! ── 为什么收尾走 trait 的网页登录钩子（而不是自建回调监听器）──────
//! 本仓的「网页登录」链路已经是通用设施：`POST /api/session/login/start`
//! 问适配器的 `build_login_url()` 拿 `(授权地址, state)`，用户在登录窗口里完成，
//! 回调 URL 再 POST 到 `/api/session/login/callback` → `submit_login_callback`
//! → 适配器的 `exchange_login_code(code, state)`（见 `core::login` 与
//! `providers::adapter` 的模块头）。Devin 的回调形态正是标准的
//! `code` / `state` 查询串，与 Accio / Antigravity 同款 —— 直接接这套钩子，
//! 不另起一套 loopback 监听器，账号落库也就复用了同一条添加路径。
//!
//! ── 待办（verifier / redirect_uri）放哪 ───────────────────────
//! 与 Accio / CodeArts 同一处：本模块的**进程级表**（不落盘、不能进任务表 ——
//! 任务表会被 `/wait` 序列化给界面，verifier 是秘密）。
//!
//! ── 关于 redirect_uri 的端口 ─────────────────────────────────
//! Devin 的 CLI 回调允许 loopback 任意端口 + `/callback` 路径（参考实现就是
//! 现开一个 `port 0` 的监听器）。本仓的登录窗口会把回调 URL 交给网关收尾，
//! 那个端口**不需要真的在监听**，所以取一个固定端口即可；关键是**发起时与换码时
//! 用的是同一个** [`redirect_uri`]。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::errors::GatewayError;

use super::credentials::Credential;
use super::endpoints::{self, CLI_VERSION};

/// 回调路径（Devin CLI 固定用 `/callback`）
pub const CALLBACK_PATH: &str = "/callback";
/// 交给上游的 loopback 端口（固定即可：真正收尾在登录窗口，不在这个端口）
pub const CALLBACK_PORT: u16 = 47_815;
/// 待办的保留时长（超过就当这一轮作废）
const PENDING_TTL_MS: i64 = 10 * 60 * 1000;

/// 交给上游的 `redirect_uri`（发起与换码必须逐字一致）。
pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// 一轮待办的登录（PKCE verifier + redirect_uri）。
#[derive(Clone)]
pub struct PendingLogin {
    pub verifier: String,
    pub redirect_uri: String,
    pub started_at_ms: i64,
}

fn pending_slot() -> &'static Mutex<HashMap<String, PendingLogin>> {
    static PENDING: OnceLock<Mutex<HashMap<String, PendingLogin>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_pending() -> std::sync::MutexGuard<'static, HashMap<String, PendingLogin>> {
    pending_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 开始一轮网页登录：返回 `(授权地址, state)`。
///
/// verifier 与 redirect_uri 记进待办表（换码时要用），state 是这一轮的键。
pub fn begin_login() -> (String, String) {
    let verifier = base64url(&random_bytes(48));
    let state = base64url(&random_bytes(24));
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let redirect = redirect_uri();
    let now = crate::server::logging::now_ms();
    {
        let mut table = lock_pending();
        table.retain(|_, pending| now - pending.started_at_ms < PENDING_TTL_MS);
        table.insert(
            state.clone(),
            PendingLogin {
                verifier,
                redirect_uri: redirect.clone(),
                started_at_ms: now,
            },
        );
    }
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("redirect_uri", &redirect);
    query.append_pair("state", &state);
    query.append_pair("prompt", "select_account");
    query.append_pair("code_challenge", &challenge);
    query.append_pair("code_challenge_method", "S256");
    // CLI 独有的标记：不带它 Devin 不会走 CLI 这条通道
    query.append_pair("cli_pkce_marker", "1");
    (format!("{}?{}", endpoints::AUTHORIZE, query.finish()), state)
}

/// 取走某一轮待办（一次性：换码只用一次）。
pub fn take_pending(state: &str) -> Option<PendingLogin> {
    lock_pending().remove(state)
}

/// 丢弃某一轮待办（取消登录 / 换码失败时调用）。
pub fn drop_pending(state: &str) -> bool {
    lock_pending().remove(state).is_some()
}

/// 用授权码换会话令牌。
pub async fn exchange_code(
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Credential, GatewayError> {
    let body = json!({
        "code": code,
        "code_verifier": verifier,
        "redirect_uri": redirect_uri,
    });
    let client = egress::client_for_system_proxy();
    let response = client
        .post(endpoints::exchange_url(endpoints::SERVER))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(30))
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| {
            GatewayError::with_status(
                502,
                format!("Devin 换码请求失败：{}", egress::describe_error_detail(&error)),
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let detail: String = text.chars().take(300).collect();
        return Err(GatewayError::with_status(
            i32::from(status),
            format!("Devin 换码失败：{status} {detail}"),
        ));
    }
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let token = parsed
        .get("sessionToken")
        .or_else(|| parsed.get("session_token"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GatewayError::with_status(502, "Devin 换码响应里没有会话令牌（sessionToken）")
        })?;
    Ok(Credential {
        token,
        api_server_url: parsed
            .get("apiServerUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        webapp_host: parsed
            .get("devinWebappHost")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        api_url: parsed
            .get("devinApiUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        email: String::new(),
    })
}

/// `whoByKey` 的结果：这枚令牌是谁的。
pub enum Identity {
    /// 认出来了（邮箱 / 展示名）
    Known(String),
    /// Devin 明确拒绝了这枚令牌（401）
    Refused,
    /// 判不出来（网络 / 解析失败）—— 不据此判定成败
    Unknown,
}

/// 问 `GetUserStatus` 这枚令牌的账号是谁（顺带做一次「令牌还有效吗」的检查）。
///
/// 与参考实现的 `whoByKey` 同一取舍：**判不出来与「被拒绝」是两件事** ——
/// 前者（网络抖动 / 上游改形态）不该把一次登录判死，后者才是真的签名无效。
pub async fn who_by_key(token: &str, base: &str) -> Identity {
    let body = json!({
        "metadata": {
            "ideName": "devin-cli",
            "ideVersion": CLI_VERSION,
            "extensionName": "devin-cli",
            "extensionVersion": CLI_VERSION,
            "apiKey": token,
            "locale": "en",
            "os": endpoints::os_name(),
        }
    });
    let client = egress::client_for_system_proxy();
    let response = match client
        .post(endpoints::user_status_url(base))
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .timeout(std::time::Duration::from_secs(15))
        .body(body.to_string())
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Identity::Unknown,
    };
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if status == 401 || super::stream::classify_failure(status, &text).status_code == 401 {
        return Identity::Refused;
    }
    if !(200..300).contains(&status) {
        return Identity::Unknown;
    }
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let user = parsed.get("userStatus").cloned().unwrap_or(Value::Null);
    let name = ["email", "name"]
        .iter()
        .find_map(|key| user.get(*key).and_then(Value::as_str))
        .map(str::to_string);
    match name {
        Some(name) if !name.is_empty() => Identity::Known(name),
        _ => Identity::Unknown,
    }
}

/// 用回调里的一次性 `code` 换凭证、认一下身份、落账号，返回账号 id。
pub async fn exchange_and_store(
    store: &AccountStore,
    code: &str,
    state: &str,
) -> Result<String, GatewayError> {
    let Some(pending) = take_pending(state) else {
        return Err(GatewayError::with_status(
            404,
            "这次 Devin 登录已取消或已过期，请重新发起",
        ));
    };
    let mut credential = exchange_code(code, &pending.verifier, &pending.redirect_uri).await?;
    match who_by_key(&credential.token, &credential.server()).await {
        Identity::Refused => {
            return Err(GatewayError::with_status(
                401,
                "Devin 拒绝了这次登录返回的令牌，请重新登录",
            ));
        }
        Identity::Known(email) => credential.email = email,
        Identity::Unknown => {}
    }
    let account = store
        .add_devin_account(&credential, None, "web")
        .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
    let account_id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    crate::server::logging::log("[Login]", "✅ Devin 网页登录完成，账号已加入列表");
    Ok(account_id)
}

/// `n` 字节的随机源
fn random_bytes(count: usize) -> Vec<u8> {
    let mut buf = vec![0u8; count];
    let _ = getrandom::getrandom(&mut buf);
    buf
}

fn sha256(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn base64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_login_builds_a_cli_pkce_authorize_url() {
        let (url, state) = begin_login();
        assert!(url.starts_with(endpoints::AUTHORIZE));
        for needle in [
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A47815%2Fcallback",
            "code_challenge_method=S256",
            "cli_pkce_marker=1",
            "prompt=select_account",
            &format!("state={state}"),
        ] {
            assert!(url.contains(needle), "授权地址缺 {needle}：{url}");
        }
        // 待办记下来了，且换码用同一份 redirect_uri
        let pending = take_pending(&state).expect("应当有待办");
        assert_eq!(redirect_uri(), pending.redirect_uri);
        assert!(!pending.verifier.is_empty());
        // 取走后不再有（一次性）
        assert!(take_pending(&state).is_none());
    }

    #[test]
    fn dropping_an_unknown_pending_round_reports_false() {
        assert!(!drop_pending("no-such-state"));
    }

    #[test]
    fn a_refused_token_classifies_as_unauthorized() {
        // 这条只验「上游错误体 → 状态码」的映射（不联网）：`classify_failure` 对
        // unauthenticated 给 401。`who_by_key` 那层「401 → Refused」的取舍要联网
        // 才能验，不在这里测 —— 所以测试名只说它真正断言的东西。
        assert_eq!(401, super::super::stream::classify_failure(200, "{\"error\":{\"code\":\"unauthenticated\",\"message\":\"bad\"}}").status_code);
    }
}
