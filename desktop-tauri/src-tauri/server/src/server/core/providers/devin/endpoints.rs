//! Devin 上游的端点常量与地址拼装（含**按账号**的 server 覆盖）。
//!
//! ── 这一家上游长什么样 ────────────────────────────────────────
//! Devin 是 Windsurf 的订阅产品，推理走它的 **Connect-RPC**
//! `exa.api_server_pb.ApiServerService/GetChatMessage`（protobuf 载荷，见 `pb.rs`），
//! 鉴权是 `Authorization: Basic <token>`（token 形如 `devin-session-token$<jwt>`）。
//! 应用服务器基址默认 `https://server.codeium.com`，但**可以按账号覆盖**
//! （CLI 的 `credentials.toml` 里有 `api_server_url` 一项，灰度 / 自建链路会把它指到
//! 别处）。所有地址都由 [`server_base`] 拼出，别处不要再写域名字面量。
//!
//! ── 与客户端一致的几个标识 ────────────────────────────────────
//! Devin 这一套是**给 CLI 用的**：请求 metadata 里要自报是 `devin-cli`、报版本号，
//! 报操作系统。版本号是硬编码的 [`CLI_VERSION`]（参考实现同款），它同时决定
//! 上游给哪一档模型清单 —— 少报或报错会让 `GetCliModelConfigs` 只回一两个模型
//! （参考实现里那句「with its ide and extension names alone Devin lists one model」）。

/// Devin 应用服务器默认基址
pub const SERVER: &str = "https://server.codeium.com";
/// CLI 网页登录（PKCE）的授权页
pub const AUTHORIZE: &str = "https://app.devin.ai/auth/cli/continue";
/// PKCE 授权码换取会话令牌的 RPC（JSON 载荷，不是 Connect 帧）
pub const EXCHANGE_RPC: &str =
    "/exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode";
/// 对话 RPC（Connect-RPC，protobuf 载荷）
pub const CHAT_RPC: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
/// 模型目录 RPC（`Content-Type: application/proto`，protobuf 载荷）
pub const MODEL_CONFIGS_RPC: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
/// 账号状态 RPC（登录后认身份用，JSON 载荷）
pub const USER_STATUS_RPC: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
/// 请求自报的 CLI 版本（同时决定上游回哪一档模型清单）
pub const CLI_VERSION: &str = "3000.11.3";
/// CLI 登录态的默认 webapp 主机（credentials.toml 的 `devin_webapp_host`）
pub const DEFAULT_WEBAPP_HOST: &str = "app.devin.ai";
/// CLI 登录态的默认 API 基址（credentials.toml 的 `devin_api_url`）
pub const DEFAULT_API_URL: &str = "https://api.devin.ai";

/// CLI 自报的操作系统名（`"windows"` / `"linux"` / `"macos"`）。
///
/// 与参考实现的 `osName()` 逐字一致：Windows 归 `windows`，macOS 归 `macos`
/// （Node 的 `process.platform` 就是 `darwin`，参考实现只对 win32 特判），
/// 其余一律 `linux`。
pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// 应用服务器基址：账号可选覆盖，缺省回 [`SERVER`]。
///
/// 去掉末尾的 `/`（参考实现的 `serverOf` 同样 `.replace(/\/+$/, "")`）——
/// 覆盖值末尾多一个斜杠会让拼出来的路径变成 `//exa.api_server_pb…`，
/// 上游对某些网关会 404，症状是「账号明明能用却每个请求都找不到端点」。
pub fn server_base(override_value: Option<&str>) -> String {
    let text = override_value.map(str::trim).unwrap_or("");
    if text.is_empty() {
        return SERVER.to_string();
    }
    text.trim_end_matches('/').to_string()
}

/// 对话 RPC 的完整 URL
pub fn chat_url(base: &str) -> String {
    format!("{base}{CHAT_RPC}")
}

/// 模型目录 RPC 的完整 URL
pub fn model_configs_url(base: &str) -> String {
    format!("{base}{MODEL_CONFIGS_RPC}")
}

/// PKCE 换码 RPC 的完整 URL
pub fn exchange_url(base: &str) -> String {
    format!("{base}{EXCHANGE_RPC}")
}

/// 账号状态 RPC 的完整 URL
pub fn user_status_url(base: &str) -> String {
    format!("{base}{USER_STATUS_RPC}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_base_trims_trailing_slashes_and_falls_back() {
        assert_eq!(SERVER, server_base(None));
        assert_eq!(SERVER, server_base(Some("   ")));
        assert_eq!(
            "https://example.invalid",
            server_base(Some("https://example.invalid/"))
        );
        assert_eq!(
            "https://example.invalid",
            server_base(Some("https://example.invalid///"))
        );
    }

    #[test]
    fn url_builders_join_without_double_slash() {
        assert_eq!(format!("{SERVER}{CHAT_RPC}"), chat_url(SERVER));
        assert_eq!(format!("{SERVER}{MODEL_CONFIGS_RPC}"), model_configs_url(SERVER));
        assert_eq!(format!("{SERVER}{EXCHANGE_RPC}"), exchange_url(SERVER));
    }

    #[test]
    fn os_name_is_one_of_the_three_known_words() {
        assert!(["windows", "linux", "macos"].contains(&os_name()));
    }
}
