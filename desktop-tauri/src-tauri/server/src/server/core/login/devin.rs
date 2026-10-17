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
//! ── 导入会验一次身份（本仓「添加账号不调上游」的一个明确例外）─────
//! 全仓的口径是「添加账号不调上游」（凭据是登录 / 导入换来的，没有可交换的授权码，
//! 见 CodeArts / OfficeAce）。本文件这条**例外**：前端「添加账号」页已经对用户承诺
//! 了「导入时会向 Devin 验一次身份并补上账号邮箱」（`ui-islands` 的 `DEVIN.desktopNote`
//! 与 i18n 的对应条目），参考实现（magpie 的 devin 插件 `cliSignIn`）也是这么做的。
//! 验身份的收益有两个：把账号邮箱补上（去重与展示都更好），以及**当场**发现「本机这
//! 枚令牌已经作废」—— 否则用户会得到一条一用就 401 的账号，而错误要等到第一次真正
//! 转发时才暴露。
//!
//! 尺度上有意收了一手：**只有上游明确拒绝（401）才算失败**。「认不出身份」
//! （`Identity::Unknown`，含网络不通 / 上游抖动）照常导入，只是不补邮箱 —— 不然
//! 一次网络抖动就会让「导入本机登录态」这个纯本地动作彻底不可用。

use crate::server::core::providers::devin::{credentials, oauth};
use crate::server::errors::GatewayError;

/// 只读读出本机 CLI 的登录态并**验一次身份**，交出凭证（供「添加账号」的导入入口
/// 直接取用，从而与手动粘贴共用同一条落账号路径）。
///
/// 失败语义见模块头：被上游拒绝 → 401；读不到文件 / TOML 解不出 → 400。
pub async fn verified_cli_credential() -> Result<credentials::Credential, GatewayError> {
    let mut credential = credentials::read_cli_credentials()
        .map_err(|reason| GatewayError::with_status(400, reason))?;
    match oauth::who_by_key(&credential.token, &credential.server()).await {
        oauth::Identity::Refused => Err(GatewayError::with_status(
            401,
            "本机 devin CLI 的登录态已被 Devin 拒绝：请重新运行 `devin auth login` 后再导入",
        )),
        oauth::Identity::Known(email) => {
            credential.email = email;
            Ok(credential)
        }
        oauth::Identity::Unknown => Ok(credential),
    }
}
