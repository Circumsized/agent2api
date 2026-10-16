//! Devin 的登录接线：本机 devin CLI 登录态导入。
//!
//! ── 为什么这里只剩一条 ──────────────────────────────────────
//!   · **网页 PKCE 登录**不需要本文件包一层：它复用了全仓通用的「网页登录」链路
//!     （`supports_web_login` + `build_login_url` + `exchange_login_code`，见
//!     `providers::devin::oauth`），由 `core::login` 的 `start_web_login(kind)` 与
//!     `submit_login_callback` 的 Devin 分支统一收尾 —— 与 Accio / Antigravity
//!     同一分工。
//!   · **导入本机 devin CLI 登录态**（**只读**）：读它写的 `credentials.toml`，取出
//!     `windsurf_api_key` 后落账号。这条路径不碰 CLI 自己的数据（不写、不改），只是
//!     把它已经登录的那枚令牌搬进网关。
//!
//! ── 导入**不调上游**（一个明确的取舍，而不是漏掉的一步）─────────
//! 参考实现（magpie 的 devin 插件）在 CLI 导入时会顺手 `GetUserStatus` 验一次身份，
//! 好处是能补上邮箱、并**当场**发现「这枚令牌已经作废」。本仓**没有**跟这一步：
//! 与 CodeArts / OfficeAce 同口径 —— 「添加账号」不调上游（见 `api::accounts` 里
//! Devin 分支的说明），凭据是登录 / 导入换来的，添加时没有可交换的授权码。
//! 代价是：导入一枚已失效的令牌会落成一条「一用就 401」的账号，要用户自己在首次
//! 调用失败后重新登录。要改口径，就把 `oauth::who_by_key` 的验身份接进下面这条
//! 入口（它已经写好，是刻意不接的）。

use crate::server::core::providers::devin::credentials;

/// 只读读出本机 CLI 的登录态，交出凭证（供「添加账号」的导入入口直接取用，
/// 从而与手动粘贴共用同一条落账号路径）。
///
/// **不验身份**：理由见模块头「导入不调上游」那一段。
pub fn cli_credential() -> Result<credentials::Credential, String> {
    credentials::read_cli_credentials()
}
