//! Devin 的登录接线：网页 PKCE 登录（走适配器钩子）+ 本机 CLI 登录态导入。
//!
//! ── 两条入口 ────────────────────────────────────────────────
//!   1. **网页 PKCE 登录**：本家没有自己的回调监听器，而是复用了全仓通用的
//!      「网页登录」链路（`supports_web_login` + `build_login_url` +
//!      `exchange_login_code`，见 `providers::devin::oauth` 与 `core::login`
//!      的模块头）。[`LoginService::start_devin_login`] 只是那条链的薄封装。
//!   2. **导入本机 devin CLI 登录态**（**只读**）：读它写的
//!      `credentials.toml`，取出 `windsurf_api_key` 并验一次身份后落账号。
//!      这条路径不碰 CLI 自己的数据（不写、不改），只是把它已经登录的那枚令牌
//!      搬进网关。
//!
//! 网页 PKCE 的换码收尾在 `core::login::submit_login_callback` 的 Devin 分支，
//! 这里不再重复实现 —— 与 Accio / Antigravity 同一分工。

use serde_json::Value;

use crate::server::core::providers::devin::{credentials, oauth};
use crate::server::core::providers::ProviderKind;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{LoginService, LoginTaskHandle};

impl LoginService {
    /// 发起 Devin 网页登录（PKCE 授权地址由适配器现造）。
    pub fn start_devin_login(&self) -> Result<LoginTaskHandle, String> {
        self.start_web_login(ProviderKind::Devin)
    }

    /// 导入本机 devin CLI 的登录态（**只读**）：验一次身份后落账号，返回公开形态。
    ///
    /// 验身份（`GetUserStatus`）不是必须的一步，但它有两个用处：把账号邮箱补上
    /// （去重与展示都更好），以及**当场**发现「本机这枚令牌已经作废」——
    /// 否则用户会得到一条一用就 401 的账号。
    pub async fn import_devin_cli_login(&self, name: Option<&str>) -> Result<Value, GatewayError> {
        let mut credential = credentials::read_cli_credentials()
            .map_err(|reason| GatewayError::with_status(400, reason))?;
        match oauth::who_by_key(&credential.token, &credential.server()).await {
            oauth::Identity::Refused => {
                return Err(GatewayError::with_status(
                    401,
                    "本机 devin CLI 的登录态已被 Devin 拒绝，请重新运行 `devin auth login`",
                ));
            }
            oauth::Identity::Known(email) => credential.email = email,
            oauth::Identity::Unknown => {}
        }
        let account = self
            .store
            .add_devin_account(&credential, name, "desktop")
            .map_err(|error| GatewayError::with_status(error.status_code, error.message))?;
        logging::log("[Login]", "✅ 已导入本机 devin CLI 的登录态");
        Ok(account)
    }
}

/// 只读导入本机 CLI 登录态，交出凭证（供「添加账号」的导入入口直接取用，
/// 从而与手动粘贴共用同一条落账号路径）。
pub fn cli_credential() -> Result<credentials::Credential, String> {
    credentials::read_cli_credentials()
}
