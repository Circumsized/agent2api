//! Devin 的模型目录：**快照表** + `GetCliModelConfigs` 的 protobuf 解析 +
//! 「对外广告形态」的折叠。
//!
//! ── 这一家的模型清单为什么不是一份平铺列表 ────────────────────
//! Devin 对外讲的是「模型家族（family）+ 变体（variant）」：一个家族
//! （`swe-2`）下有若干**档位变体**（`swe-2-low` / `swe-2-high` / `swe-2-max`…），
//! 真正发给上游的是**变体 id**，而客户端认得的是家族 id。因此本模块做三件事：
//!   1. 把 Devin 自己的目录（[`parse_config_families`]）或内置快照表
//!      （[`SNAPSHOT`]）读成同一份 `Family` 形状；
//!   2. 把它**折叠**成对下游广告的清单（[`listed`]）：每个家族只出**一条**
//!      （带它的档位集合），外加 `-fast` / `-priority` 两个快速档，以及不带档位的
//!      变体；某一族只有一条变体时那一族就用它自己的 id；
//!   3. 把客户端点名的那个名字 + 档位**还原**成真正发送的变体 id
//!      （[`variant_for`]）：家族 id 走「离请求档位最近的那条变体」，没请求档位
//!      则走家族默认。
//!
//! 这三步逐字移植参考实现 `packages/devin/index.mjs`（MIT）的
//! `parseConfigFamilies` / `listed` / `variantFor`，连判据顺序都保持原样。
//!
//! ── 快照表的地位（别把它当兜底乱塞）───────────────────────────
//! [`SNAPSHOT`] 是 Devin 在 2026-10-14 通过 `devin models list --format json`
//! 给出的那份清单（去掉 `adaptive` / `fusion` 两族 —— 它们在 Devin 自己的
//! agent 内部路由模型，API 不直接提供）。账号自己的清单来自
//! [`parse_config_families`]（`GetCliModelConfigs`），**优先于快照**；只有
//! 账号那份还没有时（首次刷新之前）才用快照把清单先立起来。

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use super::pb::{self, float32};

/// 一次变体（家族下的一个具体模型 id）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variant {
    pub id: String,
    pub name: String,
    pub context: i64,
    pub output: i64,
}

/// 一个模型家族
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Family {
    pub uid: String,
    pub label: String,
    pub aliases: Vec<String>,
    pub models: Vec<Variant>,
}

/// Devin 内置快照表（`devin models list --format json`，2026-10-14；去掉
/// adaptive / fusion）。行 = `(家族 id, 展示名, 别名, 变体[].(id, 展示名, 上下文, 输出))`。
///
/// Inkling 与 Claude Sonnet 4.5 已从 Devin 的新清单里消失，但快照里保留 ——
/// 仍被那样提供的账号要靠它们（参考实现同款）。
pub const SNAPSHOT: &[(&str, &str, &[&str], &[(&str, &str, i64, i64)])] = &[
    ("swe-2", "SWE-2", &["swe"], &[
        ("swe-2-high", "SWE-2 High", 262000, 128000),
        ("swe-2-medium", "SWE-2 Medium", 262000, 128000),
        ("swe-2-max", "SWE-2 Max", 262000, 128000),
    ]),
    ("swe-1.7-lightning", "SWE-1.7 Lightning", &[], &[
        ("swe-1-7-lightning", "SWE-1.7 Lightning Max", 202752, 96000),
        ("swe-1-7-lightning-medium", "SWE-1.7 Lightning Medium", 202752, 96000),
    ]),
    ("claude-fable-5-1", "Claude Fable 5.1", &["fable"], &[
        ("claude-fable-5-1-medium", "Claude Fable 5.1 Medium", 1000000, 128000),
        ("claude-fable-5-1-low", "Claude Fable 5.1 Low", 1000000, 128000),
        ("claude-fable-5-1-high", "Claude Fable 5.1 High", 1000000, 128000),
        ("claude-fable-5-1-xhigh", "Claude Fable 5.1 XHigh", 1000000, 128000),
        ("claude-fable-5-1-max", "Claude Fable 5.1 Max", 1000000, 128000),
    ]),
    ("claude-opus-5-5", "Claude Opus 5.5", &[], &[
        ("claude-opus-5-5-medium", "Claude Opus 5.5 Medium", 1000000, 128000),
        ("claude-opus-5-5-low", "Claude Opus 5.5 Low", 1000000, 128000),
        ("claude-opus-5-5-high", "Claude Opus 5.5 High", 1000000, 128000),
        ("claude-opus-5-5-xhigh", "Claude Opus 5.5 XHigh", 1000000, 128000),
        ("claude-opus-5-5-max", "Claude Opus 5.5 Max", 1000000, 128000),
        ("claude-opus-5-5-low-fast", "Claude Opus 5.5 Low Fast", 1000000, 128000),
        ("claude-opus-5-5-medium-fast", "Claude Opus 5.5 Medium Fast", 1000000, 128000),
        ("claude-opus-5-5-high-fast", "Claude Opus 5.5 High Fast", 1000000, 128000),
        ("claude-opus-5-5-xhigh-fast", "Claude Opus 5.5 XHigh Fast", 1000000, 128000),
        ("claude-opus-5-5-max-fast", "Claude Opus 5.5 Max Fast", 1000000, 128000),
    ]),
    ("gpt-6-astra", "GPT-6 Astra", &[], &[
        ("gpt-6-astra-medium", "GPT-6 Astra Medium Thinking", 1000000, 128000),
        ("gpt-6-astra-low", "GPT-6 Astra Low Thinking", 1000000, 128000),
        ("gpt-6-astra-high", "GPT-6 Astra High Thinking", 1000000, 128000),
        ("gpt-6-astra-xhigh", "GPT-6 Astra XHigh Thinking", 1000000, 128000),
        ("gpt-6-astra-max", "GPT-6 Astra Max Thinking", 1000000, 128000),
        ("gpt-6-astra-low-priority", "GPT-6 Astra Low Thinking Fast", 1000000, 128000),
        ("gpt-6-astra-medium-priority", "GPT-6 Astra Medium Thinking Fast", 1000000, 128000),
        ("gpt-6-astra-high-priority", "GPT-6 Astra High Thinking Fast", 1000000, 128000),
        ("gpt-6-astra-xhigh-priority", "GPT-6 Astra XHigh Thinking Fast", 1000000, 128000),
        ("gpt-6-astra-max-priority", "GPT-6 Astra Max Thinking Fast", 1000000, 128000),
    ]),
    ("gpt-6-sol", "GPT-6 Sol", &[], &[
        ("gpt-6-sol-medium", "GPT-6 Sol Medium Thinking", 1000000, 128000),
        ("gpt-6-sol-none", "GPT-6 Sol No Thinking", 1000000, 128000),
        ("gpt-6-sol-low", "GPT-6 Sol Low Thinking", 1000000, 128000),
        ("gpt-6-sol-high", "GPT-6 Sol High Thinking", 1000000, 128000),
        ("gpt-6-sol-xhigh", "GPT-6 Sol XHigh Thinking", 1000000, 128000),
        ("gpt-6-sol-max", "GPT-6 Sol Max Thinking", 1000000, 128000),
        ("gpt-6-sol-none-priority", "GPT-6 Sol No Thinking Fast", 1000000, 128000),
        ("gpt-6-sol-low-priority", "GPT-6 Sol Low Thinking Fast", 1000000, 128000),
        ("gpt-6-sol-medium-priority", "GPT-6 Sol Medium Thinking Fast", 1000000, 128000),
        ("gpt-6-sol-high-priority", "GPT-6 Sol High Thinking Fast", 1000000, 128000),
        ("gpt-6-sol-xhigh-priority", "GPT-6 Sol XHigh Thinking Fast", 1000000, 128000),
        ("gpt-6-sol-max-priority", "GPT-6 Sol Max Thinking Fast", 1000000, 128000),
    ]),
    ("gpt-6-luna", "GPT-6 Luna", &[], &[
        ("gpt-6-luna-medium", "GPT-6 Luna Medium Thinking", 1000000, 128000),
        ("gpt-6-luna-none", "GPT-6 Luna No Thinking", 1000000, 128000),
        ("gpt-6-luna-low", "GPT-6 Luna Low Thinking", 1000000, 128000),
        ("gpt-6-luna-high", "GPT-6 Luna High Thinking", 1000000, 128000),
        ("gpt-6-luna-xhigh", "GPT-6 Luna XHigh Thinking", 1000000, 128000),
        ("gpt-6-luna-max", "GPT-6 Luna Max Thinking", 1000000, 128000),
        ("gpt-6-luna-none-priority", "GPT-6 Luna No Thinking Fast", 1000000, 128000),
        ("gpt-6-luna-low-priority", "GPT-6 Luna Low Thinking Fast", 1000000, 128000),
        ("gpt-6-luna-medium-priority", "GPT-6 Luna Medium Thinking Fast", 1000000, 128000),
        ("gpt-6-luna-high-priority", "GPT-6 Luna High Thinking Fast", 1000000, 128000),
        ("gpt-6-luna-xhigh-priority", "GPT-6 Luna XHigh Thinking Fast", 1000000, 128000),
        ("gpt-6-luna-max-priority", "GPT-6 Luna Max Thinking Fast", 1000000, 128000),
    ]),
    ("gemini-4-argon", "Gemini 4 Argon", &[], &[
        ("gemini-4-argon-medium", "Gemini 4 Argon Medium", 1048576, 1000000),
        ("gemini-4-argon-low", "Gemini 4 Argon Low", 1048576, 1000000),
        ("gemini-4-argon-high", "Gemini 4 Argon High", 1048576, 1000000),
    ]),
    ("kimi-k3", "Kimi K3", &[], &[
        ("kimi-k3-high", "Kimi K3 High", 1048576, 131072),
        ("kimi-k3-low", "Kimi K3 Low", 1048576, 131072),
        ("kimi-k3-max", "Kimi K3 Max", 1048576, 131072),
    ]),
    ("glm-5.2", "GLM-5.2", &[], &[
        ("glm-5-2", "GLM-5.2 High", 200000, 128000),
        ("glm-5-2-max", "GLM-5.2 Max", 200000, 128000),
        ("glm-5-2-1m", "GLM-5.2 High 1M", 1000000, 128000),
        ("glm-5-2-max-1m", "GLM-5.2 Max 1M", 1000000, 128000),
        ("glm-5-2-none", "GLM-5.2 No Thinking", 200000, 128000),
        ("glm-5-2-none-1m", "GLM-5.2 No Thinking 1M", 1000000, 128000),
    ]),
    ("glm-5-3", "GLM-5.3", &[], &[
        ("glm-5-3-low", "GLM-5.3 Low", 1048576, 128000),
        ("glm-5-3-high", "GLM-5.3 High", 1048576, 128000),
        ("glm-5-3-max", "GLM-5.3 Max", 1048576, 128000),
    ]),
    ("claude-sonnet-5-5", "Claude Sonnet 5.5", &[], &[
        ("claude-sonnet-5-5-medium", "Claude Sonnet 5.5 Medium", 1000000, 128000),
        ("claude-sonnet-5-5-low", "Claude Sonnet 5.5 Low", 1000000, 128000),
        ("claude-sonnet-5-5-high", "Claude Sonnet 5.5 High", 1000000, 128000),
        ("claude-sonnet-5-5-xhigh", "Claude Sonnet 5.5 XHigh", 1000000, 128000),
        ("claude-sonnet-5-5-max", "Claude Sonnet 5.5 Max", 1000000, 128000),
    ]),
    ("gemini-3-8-flash", "Gemini 3.8 Flash", &["gemini"], &[
        ("gemini-3-8-flash-medium", "Gemini 3.8 Flash Medium", 1048576, 65535),
        ("gemini-3-8-flash-low", "Gemini 3.8 Flash Low", 1048576, 65535),
        ("gemini-3-8-flash-high", "Gemini 3.8 Flash High", 1048576, 65535),
    ]),
    ("claude-opus-4.7", "Claude Opus 4.7", &[], &[
        ("claude-opus-4-7-medium", "Claude Opus 4.7 Medium", 1000000, 128000),
        ("claude-opus-4-7-low", "Claude Opus 4.7 Low", 1000000, 128000),
        ("claude-opus-4-7-high", "Claude Opus 4.7 High", 1000000, 128000),
        ("claude-opus-4-7-xhigh", "Claude Opus 4.7 XHigh", 1000000, 128000),
        ("claude-opus-4-7-max", "Claude Opus 4.7 Max", 1000000, 128000),
    ]),
    ("claude-opus-4.8", "Claude Opus 4.8", &[], &[
        ("claude-opus-4-8-medium", "Claude Opus 4.8 Medium", 1000000, 128000),
        ("claude-opus-4-8-low", "Claude Opus 4.8 Low", 1000000, 128000),
        ("claude-opus-4-8-high", "Claude Opus 4.8 High", 1000000, 128000),
        ("claude-opus-4-8-xhigh", "Claude Opus 4.8 XHigh", 1000000, 128000),
        ("claude-opus-4-8-max", "Claude Opus 4.8 Max", 1000000, 128000),
        ("claude-opus-4-8-low-fast", "Claude Opus 4.8 Low Fast", 1000000, 128000),
        ("claude-opus-4-8-medium-fast", "Claude Opus 4.8 Medium Fast", 1000000, 128000),
        ("claude-opus-4-8-high-fast", "Claude Opus 4.8 High Fast", 1000000, 128000),
        ("claude-opus-4-8-xhigh-fast", "Claude Opus 4.8 XHigh Fast", 1000000, 128000),
        ("claude-opus-4-8-max-fast", "Claude Opus 4.8 Max Fast", 1000000, 128000),
    ]),
    ("claude-opus-5", "Claude Opus 5", &["opus"], &[
        ("claude-opus-5-medium", "Claude Opus 5 Medium", 1000000, 128000),
        ("claude-opus-5-low", "Claude Opus 5 Low", 1000000, 128000),
        ("claude-opus-5-high", "Claude Opus 5 High", 1000000, 128000),
        ("claude-opus-5-xhigh", "Claude Opus 5 XHigh", 1000000, 128000),
        ("claude-opus-5-max", "Claude Opus 5 Max", 1000000, 128000),
        ("claude-opus-5-low-fast", "Claude Opus 5 Low Fast", 1000000, 128000),
        ("claude-opus-5-medium-fast", "Claude Opus 5 Medium Fast", 1000000, 128000),
        ("claude-opus-5-high-fast", "Claude Opus 5 High Fast", 1000000, 128000),
        ("claude-opus-5-xhigh-fast", "Claude Opus 5 XHigh Fast", 1000000, 128000),
        ("claude-opus-5-max-fast", "Claude Opus 5 Max Fast", 1000000, 128000),
    ]),
    ("claude-5-fable", "Claude Fable 5", &[], &[
        ("claude-5-fable-low", "Claude Fable 5 Low", 1000000, 128000),
        ("claude-5-fable-medium", "Claude Fable 5 Medium", 1000000, 128000),
        ("claude-5-fable-high", "Claude Fable 5 High", 1000000, 128000),
        ("claude-5-fable-xhigh", "Claude Fable 5 XHigh", 1000000, 128000),
        ("claude-5-fable-max", "Claude Fable 5 Max", 1000000, 128000),
    ]),
    ("claude-sonnet-5", "Claude Sonnet 5", &["claude", "sonnet"], &[
        ("claude-sonnet-5-low", "Claude Sonnet 5 Low", 1000000, 128000),
        ("claude-sonnet-5-medium", "Claude Sonnet 5 Medium", 1000000, 128000),
        ("claude-sonnet-5-high", "Claude Sonnet 5 High", 1000000, 128000),
        ("claude-sonnet-5-xhigh", "Claude Sonnet 5 XHigh", 1000000, 128000),
        ("claude-sonnet-5-max", "Claude Sonnet 5 Max", 1000000, 128000),
    ]),
    ("claude-haiku-5-5", "Claude Haiku 5.5", &[], &[
        ("claude-haiku-5-5-low", "Claude Haiku 5.5 Low", 1000000, 128000),
        ("claude-haiku-5-5-medium", "Claude Haiku 5.5 Medium", 1000000, 128000),
        ("claude-haiku-5-5-high", "Claude Haiku 5.5 High", 1000000, 128000),
        ("claude-haiku-5-5-xhigh", "Claude Haiku 5.5 XHigh", 1000000, 128000),
        ("claude-haiku-5-5-max", "Claude Haiku 5.5 Max", 1000000, 128000),
    ]),
    ("gemini-3.5-flash", "Gemini 3.5 Flash", &[], &[
        ("gemini-3-5-flash-minimal", "Gemini 3.5 Flash Minimal", 1048576, 65535),
        ("gemini-3-5-flash-low", "Gemini 3.5 Flash Low", 1048576, 65535),
        ("gemini-3-5-flash-medium", "Gemini 3.5 Flash Medium", 1048576, 65535),
        ("gemini-3-5-flash-high", "Gemini 3.5 Flash High", 1048576, 65535),
    ]),
    ("gemini-3.6-flash", "Gemini 3.6 Flash", &[], &[
        ("gemini-3-6-flash-minimal", "Gemini 3.6 Flash Minimal", 1048576, 65535),
        ("gemini-3-6-flash-low", "Gemini 3.6 Flash Low", 1048576, 65535),
        ("gemini-3-6-flash-medium", "Gemini 3.6 Flash Medium", 1048576, 65535),
        ("gemini-3-6-flash-high", "Gemini 3.6 Flash High", 1048576, 65535),
    ]),
    ("gemini-3-7-flash", "Gemini 3.7 Flash", &[], &[
        ("gemini-3-7-flash-low", "Gemini 3.7 Flash Low", 1048576, 65535),
        ("gemini-3-7-flash-medium", "Gemini 3.7 Flash Medium", 1048576, 65535),
        ("gemini-3-7-flash-high", "Gemini 3.7 Flash High", 1048576, 65535),
    ]),
    ("gpt-5.6-sol", "GPT-5.6 Sol", &[], &[
        ("gpt-5-6-sol-none", "GPT-5.6 Sol No Thinking", 1000000, 128000),
        ("gpt-5-6-sol-low", "GPT-5.6 Sol Low Thinking", 1000000, 128000),
        ("gpt-5-6-sol-medium", "GPT-5.6 Sol Medium Thinking", 1000000, 128000),
        ("gpt-5-6-sol-high", "GPT-5.6 Sol High Thinking", 1000000, 128000),
        ("gpt-5-6-sol-xhigh", "GPT-5.6 Sol XHigh Thinking", 1000000, 128000),
        ("gpt-5-6-sol-max", "GPT-5.6 Sol Max Thinking", 1000000, 128000),
        ("gpt-5-6-sol-none-priority", "GPT-5.6 Sol No Thinking Fast", 1000000, 128000),
        ("gpt-5-6-sol-low-priority", "GPT-5.6 Sol Low Thinking Fast", 1000000, 128000),
        ("gpt-5-6-sol-medium-priority", "GPT-5.6 Sol Medium Thinking Fast", 1000000, 128000),
        ("gpt-5-6-sol-high-priority", "GPT-5.6 Sol High Thinking Fast", 1000000, 128000),
        ("gpt-5-6-sol-xhigh-priority", "GPT-5.6 Sol XHigh Thinking Fast", 1000000, 128000),
        ("gpt-5-6-sol-max-priority", "GPT-5.6 Sol Max Thinking Fast", 1000000, 128000),
    ]),
    ("gpt-5.6-terra", "GPT-5.6 Terra", &[], &[
        ("gpt-5-6-terra-none", "GPT-5.6 Terra No Thinking", 1000000, 128000),
        ("gpt-5-6-terra-low", "GPT-5.6 Terra Low Thinking", 1000000, 128000),
        ("gpt-5-6-terra-medium", "GPT-5.6 Terra Medium Thinking", 1000000, 128000),
        ("gpt-5-6-terra-high", "GPT-5.6 Terra High Thinking", 1000000, 128000),
        ("gpt-5-6-terra-xhigh", "GPT-5.6 Terra XHigh Thinking", 1000000, 128000),
        ("gpt-5-6-terra-max", "GPT-5.6 Terra Max Thinking", 1000000, 128000),
        ("gpt-5-6-terra-none-priority", "GPT-5.6 Terra No Thinking Fast", 1000000, 128000),
        ("gpt-5-6-terra-low-priority", "GPT-5.6 Terra Low Thinking Fast", 1000000, 128000),
        ("gpt-5-6-terra-medium-priority", "GPT-5.6 Terra Medium Thinking Fast", 1000000, 128000),
        ("gpt-5-6-terra-high-priority", "GPT-5.6 Terra High Thinking Fast", 1000000, 128000),
        ("gpt-5-6-terra-xhigh-priority", "GPT-5.6 Terra XHigh Thinking Fast", 1000000, 128000),
        ("gpt-5-6-terra-max-priority", "GPT-5.6 Terra Max Thinking Fast", 1000000, 128000),
    ]),
    ("gpt-5.6-luna", "GPT-5.6 Luna", &[], &[
        ("gpt-5-6-luna-none", "GPT-5.6 Luna No Thinking", 1000000, 128000),
        ("gpt-5-6-luna-low", "GPT-5.6 Luna Low Thinking", 1000000, 128000),
        ("gpt-5-6-luna-medium", "GPT-5.6 Luna Medium Thinking", 1000000, 128000),
        ("gpt-5-6-luna-high", "GPT-5.6 Luna High Thinking", 1000000, 128000),
        ("gpt-5-6-luna-xhigh", "GPT-5.6 Luna XHigh Thinking", 1000000, 128000),
        ("gpt-5-6-luna-max", "GPT-5.6 Luna Max Thinking", 1000000, 128000),
        ("gpt-5-6-luna-none-priority", "GPT-5.6 Luna No Thinking Fast", 1000000, 128000),
        ("gpt-5-6-luna-low-priority", "GPT-5.6 Luna Low Thinking Fast", 1000000, 128000),
        ("gpt-5-6-luna-medium-priority", "GPT-5.6 Luna Medium Thinking Fast", 1000000, 128000),
        ("gpt-5-6-luna-high-priority", "GPT-5.6 Luna High Thinking Fast", 1000000, 128000),
        ("gpt-5-6-luna-xhigh-priority", "GPT-5.6 Luna XHigh Thinking Fast", 1000000, 128000),
        ("gpt-5-6-luna-max-priority", "GPT-5.6 Luna Max Thinking Fast", 1000000, 128000),
    ]),
    ("gpt-6-1-sol", "GPT-6.1 Sol", &[], &[
        ("gpt-6-1-sol-low", "GPT-6.1 Sol Low Thinking", 1000000, 128000),
        ("gpt-6-1-sol-medium", "GPT-6.1 Sol Medium Thinking", 1000000, 128000),
        ("gpt-6-1-sol-high", "GPT-6.1 Sol High Thinking", 1000000, 128000),
        ("gpt-6-1-sol-xhigh", "GPT-6.1 Sol XHigh Thinking", 1000000, 128000),
        ("gpt-6-1-sol-max", "GPT-6.1 Sol Max Thinking", 1000000, 128000),
        ("gpt-6-1-sol-low-priority", "GPT-6.1 Sol Low Thinking Fast", 1000000, 128000),
        ("gpt-6-1-sol-medium-priority", "GPT-6.1 Sol Medium Thinking Fast", 1000000, 128000),
        ("gpt-6-1-sol-high-priority", "GPT-6.1 Sol High Thinking Fast", 1000000, 128000),
        ("gpt-6-1-sol-xhigh-priority", "GPT-6.1 Sol XHigh Thinking Fast", 1000000, 128000),
        ("gpt-6-1-sol-max-priority", "GPT-6.1 Sol Max Thinking Fast", 1000000, 128000),
    ]),
    ("grok-4.5", "Grok 4.5", &[], &[
        ("grok-4-5-low", "Grok 4.5 Low", 500000, 100000),
        ("grok-4-5-medium", "Grok 4.5 Medium", 500000, 100000),
        ("grok-4-5-high", "Grok 4.5 High", 500000, 100000),
    ]),
    ("grok-4-6", "Grok 4.6", &[], &[
        ("grok-4-6-low", "Grok 4.6 Low", 500000, 100000),
        ("grok-4-6-medium", "Grok 4.6 Medium", 500000, 100000),
        ("grok-4-6-high", "Grok 4.6 High", 500000, 100000),
        ("grok-4-6-xhigh", "Grok 4.6 XHigh", 500000, 100000),
    ]),
    ("grok-4-7", "Grok 4.7", &[], &[
        ("grok-4-7-low", "Grok 4.7 Low", 500000, 100000),
        ("grok-4-7-medium", "Grok 4.7 Medium", 500000, 100000),
        ("grok-4-7-high", "Grok 4.7 High", 500000, 100000),
        ("grok-4-7-xhigh", "Grok 4.7 XHigh", 500000, 100000),
    ]),
    ("inkling", "Inkling", &[], &[
        ("inkling-none", "Inkling None", 1048576, 131072),
        ("inkling-low", "Inkling Low", 1048576, 131072),
        ("inkling-medium", "Inkling Medium", 1048576, 131072),
        ("inkling-high", "Inkling High", 1048576, 131072),
        ("inkling-xhigh", "Inkling X-High", 1048576, 131072),
        ("inkling-max", "Inkling Max", 1048576, 131072),
    ]),
    ("glm-5-3-flash", "GLM-5.3 Flash", &[], &[
        ("glm-5-3-flash-low", "GLM-5.3 Flash Low", 1000000, 128000),
        ("glm-5-3-flash-high", "GLM-5.3 Flash High", 1000000, 128000),
        ("glm-5-3-flash-max", "GLM-5.3 Flash Max", 1000000, 128000),
    ]),
    ("deepseek-v4-flash", "DeepSeek V4 Flash", &[], &[
        ("deepseek-v4-flash-high", "DeepSeek V4 Flash High", 1048576, 384000),
        ("deepseek-v4-flash-max", "DeepSeek V4 Flash Max", 1048576, 384000),
    ]),
    ("deepseek-v4-1-flash", "DeepSeek V4.1 Flash", &[], &[
        ("deepseek-v4-1-flash-high", "DeepSeek V4.1 Flash High", 1048576, 384000),
        ("deepseek-v4-1-flash-max", "DeepSeek V4.1 Flash Max", 1048576, 384000),
    ]),
    ("swe-1.7", "SWE-1.7", &[], &[
        ("swe-1-7", "SWE-1.7 Max", 262000, 128000),
        ("swe-1-7-medium", "SWE-1.7 Medium", 262000, 128000),
    ]),
    ("claude-opus-4.6", "Claude Opus 4.6", &[], &[
        ("claude-opus-4-6", "Claude Opus 4.6", 200000, 128000),
        ("claude-opus-4-6-thinking", "Claude Opus 4.6 Thinking", 200000, 128000),
        ("claude-opus-4-6-1m", "Claude Opus 4.6 1M", 1000000, 128000),
        ("claude-opus-4-6-thinking-1m", "Claude Opus 4.6 Thinking 1M", 1000000, 128000),
    ]),
    ("gpt-5.4", "GPT-5.4", &[], &[
        ("gpt-5-4-none", "GPT-5.4 No Thinking", 272000, 128000),
        ("gpt-5-4-low", "GPT-5.4 Low Thinking", 272000, 128000),
        ("gpt-5-4-medium", "GPT-5.4 Medium Thinking", 272000, 128000),
        ("gpt-5-4-high", "GPT-5.4 High Thinking", 272000, 128000),
        ("gpt-5-4-xhigh", "GPT-5.4 XHigh Thinking", 272000, 128000),
        ("gpt-5-4-none-priority", "GPT-5.4 No Thinking Fast", 272000, 128000),
        ("gpt-5-4-low-priority", "GPT-5.4 Low Thinking Fast", 272000, 128000),
        ("gpt-5-4-medium-priority", "GPT-5.4 Medium Thinking Fast", 272000, 128000),
        ("gpt-5-4-high-priority", "GPT-5.4 High Thinking Fast", 272000, 128000),
        ("gpt-5-4-xhigh-priority", "GPT-5.4 XHigh Thinking Fast", 272000, 128000),
    ]),
    ("gpt-5.5", "GPT-5.5", &["gpt"], &[
        ("gpt-5-5-none", "GPT-5.5 No Thinking", 272000, 128000),
        ("gpt-5-5-low", "GPT-5.5 Low Thinking", 272000, 128000),
        ("gpt-5-5-medium", "GPT-5.5 Medium Thinking", 272000, 128000),
        ("gpt-5-5-high", "GPT-5.5 High Thinking", 272000, 128000),
        ("gpt-5-5-xhigh", "GPT-5.5 XHigh Thinking", 272000, 128000),
        ("gpt-5-5-none-priority", "GPT-5.5 No Thinking Fast", 272000, 128000),
        ("gpt-5-5-low-priority", "GPT-5.5 Low Thinking Fast", 272000, 128000),
        ("gpt-5-5-medium-priority", "GPT-5.5 Medium Thinking Fast", 272000, 128000),
        ("gpt-5-5-high-priority", "GPT-5.5 High Thinking Fast", 272000, 128000),
        ("gpt-5-5-xhigh-priority", "GPT-5.5 XHigh Thinking Fast", 272000, 128000),
    ]),
    ("gpt-5.4-mini", "GPT-5.4 Mini", &[], &[
        ("gpt-5-4-mini-low", "GPT-5.4 Mini Low Thinking", 400000, 128000),
        ("gpt-5-4-mini-medium", "GPT-5.4 Mini Medium Thinking", 400000, 128000),
        ("gpt-5-4-mini-high", "GPT-5.4 Mini High Thinking", 400000, 128000),
        ("gpt-5-4-mini-xhigh", "GPT-5.4 Mini XHigh Thinking", 400000, 128000),
    ]),
    ("claude-sonnet-4.6", "Claude Sonnet 4.6", &[], &[
        ("claude-sonnet-4-6", "Claude Sonnet 4.6", 200000, 128000),
        ("claude-sonnet-4-6-thinking", "Claude Sonnet 4.6 Thinking", 200000, 128000),
        ("claude-sonnet-4-6-1m", "Claude Sonnet 4.6 1M", 1000000, 128000),
        ("claude-sonnet-4-6-thinking-1m", "Claude Sonnet 4.6 Thinking 1M", 1000000, 128000),
    ]),
    ("gpt-5.2", "GPT-5.2", &[], &[
        ("MODEL_GPT_5_2_LOW", "GPT-5.2 Low Thinking", 384000, 128000),
        ("MODEL_GPT_5_2_MEDIUM", "GPT-5.2 Medium Thinking", 384000, 128000),
        ("MODEL_GPT_5_2_NONE", "GPT-5.2 No Thinking", 384000, 128000),
        ("MODEL_GPT_5_2_HIGH", "GPT-5.2 High Thinking", 384000, 128000),
        ("MODEL_GPT_5_2_XHIGH", "GPT-5.2 XHigh Thinking", 384000, 128000),
    ]),
    ("claude-opus-4.5", "Claude Opus 4.5", &[], &[
        ("MODEL_CLAUDE_4_5_OPUS", "Claude Opus 4.5", 200000, 64000),
        ("MODEL_CLAUDE_4_5_OPUS_THINKING", "Claude Opus 4.5 Thinking", 200000, 64000),
    ]),
    ("Claude Haiku 4.5", "Claude Haiku 4.5", &[], &[
        ("MODEL_PRIVATE_11", "Claude Haiku 4.5", 200000, 64000),
    ]),
    ("claude-sonnet-4.5", "Claude Sonnet 4.5", &[], &[
        ("MODEL_PRIVATE_2", "Claude Sonnet 4.5", 200000, 64000),
        ("MODEL_PRIVATE_3", "Claude Sonnet 4.5 Thinking", 200000, 64000),
    ]),
    ("GPT-4.1", "GPT-4.1", &[], &[
        ("MODEL_CHAT_GPT_4_1_2025_04_14", "GPT-4.1", 1047576, 32768),
    ]),
    ("gpt-5.1", "GPT-5.1", &[], &[
        ("MODEL_PRIVATE_12", "GPT-5.1 No Thinking", 272000, 128000),
        ("MODEL_PRIVATE_13", "GPT-5.1 Low Thinking", 272000, 128000),
        ("MODEL_PRIVATE_14", "GPT-5.1 Medium Thinking", 272000, 128000),
        ("MODEL_PRIVATE_15", "GPT-5.1 High Thinking", 272000, 128000),
    ]),
    ("gpt-5.3-codex", "GPT-5.3-Codex", &["codex"], &[
        ("gpt-5-3-codex-low", "GPT-5.3-Codex Low", 400000, 128000),
        ("gpt-5-3-codex-medium", "GPT-5.3-Codex Medium", 400000, 128000),
        ("gpt-5-3-codex-high", "GPT-5.3-Codex High", 400000, 128000),
        ("gpt-5-3-codex-xhigh", "GPT-5.3-Codex X-High", 400000, 128000),
        ("gpt-5-3-codex-low-priority", "GPT-5.3-Codex Low Fast", 400000, 128000),
        ("gpt-5-3-codex-medium-priority", "GPT-5.3-Codex Medium Fast", 400000, 128000),
        ("gpt-5-3-codex-high-priority", "GPT-5.3-Codex High Fast", 400000, 128000),
        ("gpt-5-3-codex-xhigh-priority", "GPT-5.3-Codex XHigh Fast", 400000, 128000),
    ]),
    ("kimi-k2.6", "Kimi K2.6", &[], &[
        ("kimi-k2-6", "Kimi K2.6", 262144, 8192),
    ]),
    ("kimi-k2.7", "Kimi K2.7", &[], &[
        ("kimi-k2-7", "Kimi K2.7", 262144, 16000),
    ]),
    ("nemotron-3-ultra", "Nemotron 3 Ultra", &[], &[
        ("nemotron-3-ultra-none", "Nemotron 3 Ultra None", 1000000, 32768),
        ("nemotron-3-ultra-medium", "Nemotron 3 Ultra Medium", 1000000, 32768),
        ("nemotron-3-ultra-high", "Nemotron 3 Ultra High", 1000000, 32768),
    ]),
    ("swe-1.6", "SWE-1.6", &[], &[
        ("swe-1-6", "SWE-1.6", 200000, 128000),
    ]),
    ("swe-1.6-fast", "SWE-1.6 Fast", &[], &[
        ("swe-1-6-fast", "SWE-1.6 Fast", 200000, 128000),
    ]),
    ("gemini-3.1-pro", "Gemini 3.1 Pro", &[], &[
        ("gemini-3-1-pro-low", "Gemini 3.1 Pro Low Thinking", 1048576, 65535),
        ("gemini-3-1-pro-high", "Gemini 3.1 Pro High Thinking", 1048576, 65535),
    ]),
    ("gemini-3.0-flash", "Gemini 3 Flash", &[], &[
        ("MODEL_GOOGLE_GEMINI_3_0_FLASH_MINIMAL", "Gemini 3 Flash Minimal", 1048576, 65535),
        ("MODEL_GOOGLE_GEMINI_3_0_FLASH_LOW", "Gemini 3 Flash Low", 1048576, 65535),
        ("MODEL_GOOGLE_GEMINI_3_0_FLASH_MEDIUM", "Gemini 3 Flash Medium", 1048576, 65535),
        ("MODEL_GOOGLE_GEMINI_3_0_FLASH_HIGH", "Gemini 3 Flash High", 1048576, 65535),
    ]),
    ("deepseek-v4-pro", "DeepSeek V4 Pro", &[], &[
        ("deepseek-v4-pro-high", "DeepSeek V4 Pro High", 1048576, 384000),
        ("deepseek-v4-pro-max", "DeepSeek V4 Pro Max", 1048576, 384000),
    ]),
];

/// 把快照表读成 `Family` 形状
pub fn snapshot_families() -> Vec<Family> {
    SNAPSHOT
        .iter()
        .map(|(uid, label, aliases, variants)| Family {
            uid: (*uid).to_string(),
            label: (*label).to_string(),
            aliases: aliases.iter().map(|alias| (*alias).to_string()).collect(),
            models: variants
                .iter()
                .map(|(id, name, context, output)| Variant {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    context: *context,
                    output: *output,
                })
                .collect(),
        })
        .collect()
}

// ─── 档位判定（levelOf / tierOf / byLevel / efforts / tiers / nearest）──────

/// 档位强弱次序（参考实现的 LEVEL_RANK）
const LEVEL_RANK: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
/// 档位词 → 归一的档位（顺序重要：先匹配到先算，见参考实现）
const LEVELS: [(&str, &str); 8] = [
    ("none", "none"),
    ("xhigh", "xhigh"),
    ("minimal", "minimal"),
    ("min", "minimal"),
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("max", "max"),
];
/// 变体 id 在档位之后还能带的「更快一档」词（`claude-opus-5-5-high-fast`）
const TIERS: [(&str, &str); 2] = [("fast", "Fast"), ("priority", "Priority")];

/// 变体 id 自报的档位（`""` = 没说）。
pub fn level_of(id: &str) -> &'static str {
    let low = id.to_lowercase();
    for (word, level) in LEVELS {
        if low.ends_with(&format!("-{word}")) || low.ends_with(&format!("_{word}")) {
            return level;
        }
    }
    ""
}

/// 变体 id 所处的快速档与其档位：`claude-opus-5-5-high-fast` → `("fast", "high")`，
/// `swe-2-high` → `("", "high")`；带快速档词但前面没有档位的（`swe-1-6-fast`）算无档。
pub fn tier_of(id: &str) -> (&'static str, &'static str) {
    let low = id.to_lowercase();
    for (word, _) in TIERS {
        for separator in ["-", "_"] {
            let suffix = format!("{separator}{word}");
            if low.ends_with(&suffix) {
                let head = &low[..low.len() - suffix.len()];
                let level = level_of(head);
                if !level.is_empty() {
                    return (word, level);
                }
            }
        }
    }
    ("", level_of(id))
}

/// 家族在某快速档里「每个档位的第一条变体」。
fn by_level(family: &Family, tier: &str) -> HashMap<&'static str, String> {
    let mut out: HashMap<&'static str, String> = HashMap::new();
    for model in &family.models {
        let (model_tier, level) = tier_of(&model.id);
        if model_tier == tier && !level.is_empty() && model.id != family.uid && !out.contains_key(level)
        {
            out.insert(level, model.id.clone());
        }
    }
    out
}

/// 家族在某快速档有哪些档位（按 [`LEVEL_RANK`] 排序）。
fn efforts(family: &Family, tier: &str) -> Vec<&'static str> {
    let by = by_level(family, tier);
    LEVEL_RANK
        .iter()
        .copied()
        .filter(|level| by.contains_key(level))
        .collect()
}

/// 家族有哪些快速档（有变体的才算）。
fn tiers(family: &Family) -> Vec<&'static str> {
    TIERS
        .iter()
        .map(|(word, _)| *word)
        .filter(|tier| !by_level(family, tier).is_empty())
        .collect()
}

/// 家族默认档位：家族 id 点的变体（第一条）的档位；那条没档位就算 `medium`。
fn default_level(family: &Family) -> &'static str {
    if let Some(first) = family.models.first() {
        let (tier, level) = tier_of(&first.id);
        if tier.is_empty() && !level.is_empty() {
            return level;
        }
    }
    "medium"
}

/// 档位集合里离 `want` 最近的一个（**平局往上**）。
fn nearest(want: &'static str, levels: &[&'static str]) -> &'static str {
    let Some(at) = LEVEL_RANK.iter().position(|level| *level == want) else {
        return want;
    };
    if levels.is_empty() || levels.contains(&want) {
        return want;
    }
    let mut best = want;
    let mut distance = LEVEL_RANK.len();
    for level in levels {
        let index = LEVEL_RANK.iter().position(|item| item == level).unwrap_or(0);
        let delta = index.abs_diff(at);
        if delta < distance || (delta == distance && index > at) {
            best = level;
            distance = delta;
        }
    }
    best
}

/// 家族是不是某个名字（家族 id 或别名）。
fn named(family: &Family, id: &str) -> bool {
    family.uid == id || family.aliases.iter().any(|alias| alias == id)
}

/// 客户端点名的名字 + 档位 → **真正发给上游的变体 id**。
///
/// 规则（逐字移植参考实现）：
///   - 点到家族 id / 别名时：有请求档位就在「该族无快速档」的档位变体里找最近的；
///     没有请求档位就发该族的第一条变体；
///   - 点到「家族 + 快速档」（`opus-fast`）时：在那一档的变体里找最近档位；
///   - 其余（客户端直接点某条变体 id）原样发出。
pub fn variant_for(families: &[Family], model: &str, effort: &str) -> String {
    for family in families {
        if !named(family, model) || family.models.is_empty() {
            continue;
        }
        if !effort.is_empty() {
            let by = by_level(family, "");
            let levels = efforts(family, "");
            if let Some(id) = by.get(nearest(leak_level(effort), &levels)) {
                return id.clone();
            }
        }
        return family.models[0].id.clone();
    }
    for family in families {
        for tier in tiers(family) {
            let suffix = format!("-{tier}");
            let Some(head) = model.strip_suffix(&suffix) else {
                continue;
            };
            if !named(family, head) {
                continue;
            }
            let by = by_level(family, tier);
            let levels = efforts(family, tier);
            let want = if effort.is_empty() {
                default_level(family)
            } else {
                leak_level(effort)
            };
            return by
                .get(nearest(want, &levels))
                .or_else(|| levels.first().and_then(|level| by.get(level)))
                .cloned()
                .unwrap_or_else(|| model.to_string());
        }
    }
    model.to_string()
}

/// `nearest` 要的是 `&'static str` 档位；把请求档位字符串映射成那套静态值。
///
/// 认不出（自定义档位）时回一个 `medium`（家族默认档位）——**绝不用原始字符串**去
/// 查表（那一定会落空，然后发一条上游不认的 id）。`none`/`minimal` 原样保留，
/// 与参考实现「none 与 minimal 按原样走最近档」一致。
fn leak_level(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "xhigh",
        "max" => "max",
        _ => "medium",
    }
}

/// 客户端的 `reasoning_effort` → Devin 认的档位（`""` = 请求没指定）。
///
/// `ultra` 归 `max`（参考实现同款）；其余表外的值一律 `""`（不猜）。
pub fn effort_of(raw: &str) -> String {
    let effort = raw.trim().to_ascii_lowercase();
    if LEVEL_RANK.contains(&effort.as_str()) {
        return effort;
    }
    if effort == "ultra" {
        return "max".to_string();
    }
    String::new()
}

// ─── GetCliModelConfigs（protobuf）解析 ─────────────────────────────

/// 读 `GetCliModelConfigsResponse` 里每个模型的「收不收图」：`client_model_configs`(1)
/// 各自带 `model_uid`(22) 与 `supports_images`(5)。一条都没有时返回 None
/// （上层据此回落到快照的快照图片表）。
pub fn parse_model_configs(bytes: &[u8]) -> Option<HashMap<String, bool>> {
    let mut out = HashMap::new();
    for field in pb::fields(bytes) {
        if field.num != 1 || field.wire != 2 {
            continue;
        }
        let mut uid = String::new();
        let mut images = false;
        for inner in pb::fields(&field.data) {
            if inner.num == 22 && inner.wire == 2 {
                uid = String::from_utf8_lossy(&inner.data).to_string();
            } else if inner.num == 5 && inner.wire == 0 {
                images = inner.n != 0;
            }
        }
        if !uid.is_empty() {
            out.insert(uid, images);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 「账号额度不计」的模型档位值（`model_cost_tier` 的 4 = Free）。CLI 的选择器把
/// 档位标成 Low / Med / High / Free（1..4）。
const FREE_TIER: u64 = 4;

/// 读同一份响应里「免费」的模型 id（`model_cost_tier`(24) == 4）。
pub fn parse_free(bytes: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    for field in pb::fields(bytes) {
        if field.num != 1 || field.wire != 2 {
            continue;
        }
        let mut uid = String::new();
        let mut tier = 0u64;
        for inner in pb::fields(&field.data) {
            if inner.num == 22 && inner.wire == 2 {
                uid = String::from_utf8_lossy(&inner.data).to_string();
            } else if inner.num == 24 && inner.wire == 0 {
                tier = inner.n;
            }
        }
        if !uid.is_empty() && tier == FREE_TIER {
            out.insert(uid);
        }
    }
    out
}

/// 读同一份响应里每个模型的标价（USD / 1M tokens）。
///
/// 每条 config 的 `model_uid`(22) 与 `prices`(32)：每个 price 有 label(1:
/// `Input` / `Cached input` / `Output`)、float32 值(2)、单位(3: `1M tokens`)。
/// 只有单位是 `1M tokens`、且输入或输出大于 0 的模型才计一条。
pub fn parse_prices(bytes: &[u8]) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    for field in pb::fields(bytes) {
        if field.num != 1 || field.wire != 2 {
            continue;
        }
        let mut uid = String::new();
        let mut input = 0.0f64;
        let mut output = 0.0f64;
        let mut read = 0.0f64;
        for inner in pb::fields(&field.data) {
            if inner.num == 22 && inner.wire == 2 {
                uid = String::from_utf8_lossy(&inner.data).to_string();
            } else if inner.num == 32 && inner.wire == 2 {
                let mut label = String::new();
                let mut unit = String::new();
                let mut value = f64::NAN;
                for price in pb::fields(&inner.data) {
                    if price.num == 1 && price.wire == 2 {
                        label = String::from_utf8_lossy(&price.data).to_string();
                    } else if price.num == 3 && price.wire == 2 {
                        unit = String::from_utf8_lossy(&price.data).to_string();
                    } else if price.num == 2 && price.wire == 5 {
                        value = float32(&price.data).map(f64::from).unwrap_or(f64::NAN);
                    }
                }
                let slot = match label.as_str() {
                    "Input" => Some(&mut input),
                    "Cached input" => Some(&mut read),
                    "Output" => Some(&mut output),
                    _ => None,
                };
                if let Some(slot) = slot {
                    if unit == "1M tokens" && value.is_finite() && value >= 0.0 {
                        *slot = round_precision6(value);
                    }
                }
            }
        }
        if !uid.is_empty() && (input > 0.0 || output > 0.0) {
            out.insert(
                uid,
                json!({
                    "input": input,
                    "output": output,
                    "cache": { "read": read },
                }),
            );
        }
    }
    out
}

/// JS `Number(v.toPrecision(6))`：保留 6 位有效数字。
fn round_precision6(value: f64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let magnitude = value.abs().log10().floor();
    let factor = 10f64.powf(5.0 - magnitude);
    (value * factor).round() / factor
}

/// 把同一份响应读成 `devin models list` 会给出的家族形状（保持响应里的顺序）。
///
/// 每条 config：label(1)、context(18)、`model_info`(23) 里的 output(13) 与
/// family_uid(23)；家族 label 取 `family_metadata`(30) 的第一个字段。
/// 没报家族的 config 自成一家（用它的 label），`adaptive` / `fusion` 两族丢掉。
/// 别名是 CLI 自己的口径，从快照里同 id 的家族取。
pub fn parse_config_families(bytes: &[u8]) -> Vec<Family> {
    let mut order: Vec<String> = Vec::new();
    let mut families: HashMap<String, Family> = HashMap::new();
    for field in pb::fields(bytes) {
        if field.num != 1 || field.wire != 2 {
            continue;
        }
        let mut uid = String::new();
        let mut name = String::new();
        let mut context = 0i64;
        let mut output = 0i64;
        let mut family_uid = String::new();
        let mut family_label = String::new();
        for inner in pb::fields(&field.data) {
            match (inner.num, inner.wire) {
                (22, 2) => uid = String::from_utf8_lossy(&inner.data).to_string(),
                (1, 2) => name = String::from_utf8_lossy(&inner.data).to_string(),
                (18, 0) => context = inner.n as i64,
                (23, 2) => {
                    for info in pb::fields(&inner.data) {
                        if info.num == 13 && info.wire == 0 {
                            output = info.n as i64;
                        } else if info.num == 23 && info.wire == 2 {
                            family_uid = String::from_utf8_lossy(&info.data).to_string();
                        }
                    }
                }
                (30, 2) => {
                    for meta in pb::fields(&inner.data) {
                        if meta.num == 1 && meta.wire == 2 {
                            family_label = String::from_utf8_lossy(&meta.data).to_string();
                        }
                    }
                }
                _ => {}
            }
        }
        if uid.is_empty() {
            continue;
        }
        let id = if !family_uid.is_empty() {
            family_uid
        } else if !family_label.is_empty() {
            family_label.clone()
        } else if !name.is_empty() {
            name.clone()
        } else {
            uid.clone()
        };
        let low = id.to_lowercase();
        if low == "adaptive" || low == "fusion" {
            continue;
        }
        if !families.contains_key(&id) {
            let known = SNAPSHOT.iter().find(|(uid, ..)| *uid == id);
            order.push(id.clone());
            families.insert(
                id.clone(),
                Family {
                    uid: id.clone(),
                    label: if !family_label.is_empty() {
                        family_label
                    } else if !name.is_empty() {
                        name.clone()
                    } else {
                        id.clone()
                    },
                    aliases: known
                        .map(|(_, _, aliases, _)| {
                            aliases.iter().map(|alias| (*alias).to_string()).collect()
                        })
                        .unwrap_or_default(),
                    models: Vec::new(),
                },
            );
        }
        if let Some(family) = families.get_mut(&id) {
            family.models.push(Variant {
                name: if name.is_empty() { uid.clone() } else { name },
                id: uid,
                context,
                output,
            });
        }
    }
    order
        .into_iter()
        .filter_map(|id| families.remove(&id))
        .collect()
}

// ─── 广告形态折叠（listed）─────────────────────────────────────

/// 折叠后的一条对外模型条目。
#[derive(Clone, Debug)]
pub struct ListedModel {
    pub id: String,
    pub name: String,
    pub context: i64,
    pub output: i64,
    /// 该条目支持哪些档位（客户端按它渲染档位选择）
    pub efforts: Vec<&'static str>,
    /// 收不收图（`None` = Devin 没说，前端按「没说」处理）
    pub image: Option<bool>,
    /// 该条目代表的所有变体都是「额度不计」。
    ///
    /// 目录接口会给出它（[`parse_free`]），但**当前没有消费者**：`list_values` 不下发
    /// 免费 / 标价字段（方案明确不做余额）。留着是为了与参考实现的解析保持同构，
    /// 将来要出「免费标记」时有现成字段可用 —— 真要出，记得连 [`list_values`] 一起改。
    pub free: bool,
    /// 各变体标价一致时的标价（`None` = 不一致或没标价）。同样**当前无消费者**。
    pub cost: Option<Value>,
}

/// Devin 说「不收图」的变体 id（截至 2026-10-05）。快照里其余的变体都收图 ——
/// 这是在问过 Devin 之前（或问不到时）告诉清单的默认值。
pub const NO_IMAGES: &[&str] = &[
    "glm-5-2",
    "glm-5-2-max",
    "glm-5-2-1m",
    "glm-5-2-max-1m",
    "glm-5-2-none",
    "glm-5-2-none-1m",
    "glm-5-3-low",
    "glm-5-3-high",
    "glm-5-3-max",
    "inkling-none",
    "inkling-low",
    "inkling-medium",
    "inkling-high",
    "inkling-xhigh",
    "inkling-max",
    "deepseek-v4-flash-high",
    "deepseek-v4-flash-max",
    "deepseek-v4-pro-high",
    "deepseek-v4-pro-max",
    "nemotron-3-ultra-none",
    "nemotron-3-ultra-medium",
    "nemotron-3-ultra-high",
];

/// 快照每个变体的收图默认值（`SNAPSHOT_IMAGES`）。
pub fn snapshot_images() -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for (_, _, _, variants) in SNAPSHOT {
        for (id, ..) in *variants {
            out.insert((*id).to_string(), !NO_IMAGES.contains(id));
        }
    }
    out
}

/// 一个家族的变体们「一致地」说收不收图（说了不同的或都没说 → `None`）。
fn family_images(family: &Family, images: &HashMap<String, bool>) -> Option<bool> {
    let mut said: Option<bool> = None;
    for model in &family.models {
        let Some(value) = images.get(&model.id).copied() else {
            continue;
        };
        if let Some(previous) = said {
            if previous != value {
                return None;
            }
        }
        said = Some(value);
    }
    said
}

/// 折叠成对外清单（参考实现 `listed`）。
///
/// `keep` 是「用户点过的变体 id」，会额外保留各自一条（本仓当前不传）。
pub fn listed(
    families: &[Family],
    keep: &[String],
    images: &HashMap<String, bool>,
    free: &HashSet<String>,
    prices: &HashMap<String, Value>,
) -> Vec<ListedModel> {
    let all_free = |ids: &[String]| !ids.is_empty() && ids.iter().all(|id| free.contains(id));
    let priced = |ids: &[String]| -> Option<Value> {
        if ids.is_empty() {
            return None;
        }
        let first = prices.get(&ids[0])?;
        if ids.iter().any(|id| prices.get(id).map(|value| value != first).unwrap_or(true)) {
            return None;
        }
        Some(first.clone())
    };
    // 某快速档下「该条目代表的所有变体 id」（含家族默认那条）
    let own = |family: &Family, tier: &str| -> Vec<String> {
        let mut ids: Vec<String> = by_level(family, tier).into_values().collect();
        if tier.is_empty() {
            if let Some(first) = family.models.first() {
                if !ids.contains(&first.id) {
                    ids.push(first.id.clone());
                }
            }
        }
        ids
    };

    let mut out: Vec<ListedModel> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for family in families {
        let context = family
            .models
            .iter()
            .find(|model| model.context > 0)
            .map(|model| model.context)
            .unwrap_or(0);
        let output = family
            .models
            .iter()
            .find(|model| model.output > 0)
            .map(|model| model.output)
            .unwrap_or(0);
        let image = family_images(family, images);
        push_unique(
            &mut out,
            &mut seen,
            ListedModel {
                id: family.uid.clone(),
                name: family.label.clone(),
                context,
                output,
                efforts: efforts(family, ""),
                image,
                free: all_free(&own(family, "")),
                cost: priced(&own(family, "")),
            },
        );
        for tier in tiers(family) {
            let id = format!("{}-{}", family.uid, tier);
            if families.iter().any(|other| named(other, &id)) {
                continue;
            }
            let label = TIERS
                .iter()
                .find(|(word, _)| *word == tier)
                .map(|(_, label)| *label)
                .unwrap_or(tier);
            push_unique(
                &mut out,
                &mut seen,
                ListedModel {
                    id,
                    name: format!("{} {}", family.label, label),
                    context,
                    output,
                    efforts: efforts(family, tier),
                    image,
                    free: all_free(&own(family, tier)),
                    cost: priced(&own(family, tier)),
                },
            );
        }
        if family.models.len() == 1 {
            continue;
        }
        for model in &family.models {
            if model.id == family.uid || !tier_of(&model.id).1.is_empty() {
                continue;
            }
            push_unique(
                &mut out,
                &mut seen,
                ListedModel {
                    id: model.id.clone(),
                    name: model.name.clone(),
                    context: if model.context > 0 { model.context } else { context },
                    output: if model.output > 0 { model.output } else { output },
                    efforts: Vec::new(),
                    image: images.get(&model.id).copied(),
                    free: free.contains(&model.id),
                    cost: prices.get(&model.id).cloned(),
                },
            );
        }
    }
    for id in keep {
        if seen.contains(id) {
            continue;
        }
        let found = families
            .iter()
            .find_map(|family| family.models.iter().find(|model| model.id == *id).map(|model| (family, model)));
        let level = tier_of(id).1;
        let (name, context, output) = match found {
            Some((_, model)) => (model.name.clone(), model.context, model.output),
            None => {
                if level.is_empty() {
                    continue;
                }
                (id.clone(), 0, 0)
            }
        };
        push_unique(
            &mut out,
            &mut seen,
            ListedModel {
                id: id.clone(),
                name,
                context,
                output,
                efforts: if level.is_empty() { Vec::new() } else { vec![level] },
                image: images.get(id).copied(),
                free: free.contains(id),
                cost: prices.get(id).cloned(),
            },
        );
    }
    out
}

/// 按 id 去重地追加一条（先到先得）。
fn push_unique(out: &mut Vec<ListedModel>, seen: &mut HashSet<String>, model: ListedModel) {
    if seen.contains(&model.id) {
        return;
    }
    seen.insert(model.id.clone());
    out.push(model);
}

// ─── 进程级目录缓存 + 对外清单 ─────────────────────────────────

/// 一次刷新落下的目录（账号自己的清单；空 = 还在用快照）。
#[derive(Clone, Default)]
pub struct Catalog {
    pub families: Vec<Family>,
    pub images: HashMap<String, bool>,
    pub free: HashSet<String>,
    pub prices: HashMap<String, Value>,
    /// 这次拉取的时刻（毫秒；0 = 从没拉过）—— 界面「更新日期」列读它
    pub fetched_at_ms: i64,
}

static CACHED: OnceLock<Mutex<Option<Catalog>>> = OnceLock::new();

fn cache_slot() -> &'static Mutex<Option<Catalog>> {
    CACHED.get_or_init(|| Mutex::new(None))
}

/// 落一份账号目录（刷新链路调用）。
pub fn store_catalog(catalog: Catalog) {
    let now = crate::server::logging::now_ms();
    let mut slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(Catalog {
        fetched_at_ms: now,
        ..catalog
    });
}

/// 读当前目录（账号那份优先，没有则空）。
pub fn cached_catalog() -> Option<Catalog> {
    let slot = cache_slot().lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    slot.clone()
}

/// 目录是不是**远程拉到过**（缓存有账号那份清单才算；快照不算）。
pub fn remote_refreshed() -> bool {
    cached_catalog()
        .map(|catalog| !catalog.families.is_empty())
        .unwrap_or(false)
}

/// 目录最近一次远程拉取的时刻（毫秒；0 = 从没拉过）。
pub fn last_refreshed_at() -> i64 {
    cached_catalog()
        .map(|catalog| catalog.fetched_at_ms)
        .unwrap_or(0)
}

/// 转发与清单要用到的家族集合：账号那份优先，否则快照。
pub fn current_families() -> Vec<Family> {
    match cached_catalog() {
        Some(catalog) if !catalog.families.is_empty() => catalog.families,
        _ => snapshot_families(),
    }
}

/// 对外清单的**原始形态记录**（`ProviderAdapter::list_models` 的返回）。
///
/// 字段名与聚合层 `models::list_item` 对齐（`maxInputTokens` / `maxOutputTokens` /
/// `supportsImages` / `supportsToolCall` / `supportsReasoning`）。
pub fn list_values() -> Vec<Value> {
    let catalog = cached_catalog();
    let (families, images, free, prices) = match &catalog {
        Some(catalog) if !catalog.families.is_empty() => (
            catalog.families.clone(),
            catalog.images.clone(),
            catalog.free.clone(),
            catalog.prices.clone(),
        ),
        _ => (snapshot_families(), snapshot_images(), HashSet::new(), HashMap::new()),
    };
    listed(&families, &[], &images, &free, &prices)
        .iter()
        .map(|model| {
            let mut entry = serde_json::Map::new();
            entry.insert("id".to_string(), Value::String(model.id.clone()));
            entry.insert("name".to_string(), Value::String(model.name.clone()));
            if model.context > 0 {
                entry.insert("maxInputTokens".to_string(), Value::from(model.context));
            }
            if model.output > 0 {
                entry.insert("maxOutputTokens".to_string(), Value::from(model.output));
            }
            // 收不收图：Devin 明说就照它，没说就按快照默认（未知 id 默认收图，
            // 与参考实现「问不到时按快照告诉清单」同一取向）
            entry.insert(
                "supportsImages".to_string(),
                Value::Bool(model.image.unwrap_or(true)),
            );
            entry.insert("supportsToolCall".to_string(), Value::Bool(true));
            entry.insert(
                "supportsReasoning".to_string(),
                Value::Bool(!model.efforts.is_empty()),
            );
            entry.insert("kind".to_string(), Value::String("chat".to_string()));
            Value::Object(entry)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! 解析器的验收：手工搭 protobuf 字节喂进去，逐字段对。全部离线、不联网。
    use super::*;
    use crate::server::core::providers::devin::pb::Pb;

    /// 造一条 `ClientModelConfig`：uid(22) + 可选 images(5)/tier(24)/prices(32)/
    /// label(1)/context(18)/model_info(23)/family_metadata(30)。
    struct Config {
        uid: &'static str,
        images: Option<bool>,
        tier: Option<u64>,
        label: &'static str,
        context: u64,
        output: u64,
        family_uid: &'static str,
        family_label: &'static str,
        prices: Vec<(&'static str, f32, &'static str)>,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                uid: "",
                images: None,
                tier: None,
                label: "",
                context: 0,
                output: 0,
                family_uid: "",
                family_label: "",
                prices: Vec::new(),
            }
        }
    }

    fn encode_config(config: &Config) -> Vec<u8> {
        let mut pb = pb::Pb::new();
        pb.str(22, config.uid);
        if let Some(images) = config.images {
            pb.varint(5, u64::from(images));
        }
        if let Some(tier) = config.tier {
            pb.varint(24, tier);
        }
        if !config.label.is_empty() {
            pb.str(1, config.label);
        }
        if config.context > 0 {
            pb.varint(18, config.context);
        }
        if config.output > 0 || !config.family_uid.is_empty() {
            let mut info = Pb::new();
            if config.output > 0 {
                info.varint(13, config.output);
            }
            if !config.family_uid.is_empty() {
                info.str(23, config.family_uid);
            }
            pb.bytes(23, &info.done());
        }
        if !config.family_label.is_empty() {
            let mut meta = Pb::new();
            meta.str(1, config.family_label);
            pb.bytes(30, &meta.done());
        }
        for (label, value, unit) in &config.prices {
            let mut price = Pb::new();
            price.str(1, label).float(2, *value).str(3, unit);
            pb.bytes(32, &price.done());
        }
        pb.done()
    }

    fn response(configs: &[Config]) -> Vec<u8> {
        let mut pb = pb::Pb::new();
        for config in configs {
            pb.bytes(1, &encode_config(config));
        }
        pb.done()
    }

    #[test]
    fn model_configs_and_free_and_prices_read_the_same_response() {
        let bytes = response(&[
            Config {
                uid: "swe-2-high",
                images: Some(true),
                tier: Some(4),
                label: "SWE-2 High",
                context: 262000,
                output: 128000,
                family_uid: "swe-2",
                family_label: "SWE-2",
                prices: vec![],
            },
            Config {
                uid: "gpt-5-5-none",
                // proto3 省略 false：不写 5 就是 false
                images: None,
                tier: Some(2),
                label: "GPT-5.5 No Thinking",
                context: 272000,
                output: 128000,
                family_uid: "gpt-5.5",
                family_label: "GPT-5.5",
                prices: vec![
                    ("Input", 1.25, "1M tokens"),
                    ("Cached input", 0.125, "1M tokens"),
                    ("Output", 10.0, "1M tokens"),
                ],
            },
        ]);

        let images = parse_model_configs(&bytes).expect("应当读到两条");
        assert_eq!(Some(true), images.get("swe-2-high").copied());
        assert_eq!(Some(false), images.get("gpt-5-5-none").copied());

        let free = parse_free(&bytes);
        assert!(free.contains("swe-2-high"), "tier 4 就是免费");
        assert!(!free.contains("gpt-5-5-none"), "tier 2 不是免费");

        let prices = parse_prices(&bytes);
        let price = &prices["gpt-5-5-none"];
        assert_eq!(1.25, price["input"].as_f64().unwrap());
        assert_eq!(10.0, price["output"].as_f64().unwrap());
        assert_eq!(0.125, price["cache"]["read"].as_f64().unwrap());
        assert!(!prices.contains_key("swe-2-high"), "免费模型不标价");
    }

    #[test]
    fn config_families_group_variants_and_drop_adaptive() {
        let bytes = response(&[
            Config {
                uid: "swe-2-high",
                label: "SWE-2 High",
                context: 262000,
                output: 128000,
                family_uid: "swe-2",
                family_label: "SWE-2",
                ..Default::default()
            },
            Config {
                uid: "swe-2-medium",
                label: "SWE-2 Medium",
                context: 262000,
                output: 128000,
                family_uid: "swe-2",
                family_label: "SWE-2",
                ..Default::default()
            },
            Config {
                uid: "MODEL_PRIVATE_11",
                label: "Claude Haiku 4.5",
                context: 200000,
                output: 64000,
                ..Default::default()
            },
            Config {
                uid: "adaptive-x",
                family_uid: "adaptive",
                ..Default::default()
            },
        ]);
        let families = parse_config_families(&bytes);
        assert_eq!(2, families.len(), "adaptive 要丢掉，没报家族的自成一家");
        let swe = families.iter().find(|f| f.uid == "swe-2").expect("swe-2 在这");
        assert_eq!("SWE-2", swe.label);
        assert_eq!(2, swe.models.len(), "两条变体归到一族");
        assert_eq!(vec!["swe".to_string()], swe.aliases, "别名从快照取");
        // 没报家族的 config 以其 **label** 自成一家（CLI 的口径），模型 id 仍是
        // 它自己的 uid（MODEL_PRIVATE_11）
        let lone = families
            .iter()
            .find(|f| f.uid == "Claude Haiku 4.5")
            .expect("没报家族的用 label 自成一家");
        assert_eq!(1, lone.models.len());
        assert_eq!("MODEL_PRIVATE_11", lone.models[0].id);
    }

    #[test]
    fn a_whole_toml_free_double_survives_precision6() {
        // float32 的 1.4 实际是 1.39999997…：round_precision6 要把它收成 1.4
        assert!((round_precision6(f64::from(1.4f32)) - 1.4).abs() < 1e-9);
        assert!((round_precision6(0.0125) - 0.0125).abs() < 1e-12);
    }

    #[test]
    fn listing_collapses_families_and_picks_the_variant_sent_upstream() {
        let families = snapshot_families();
        let images = snapshot_images();
        let listed_models = listed(&families, &[], &images, &HashSet::new(), &HashMap::new());
        // swe-2 一族折叠成一条（id 就是家族 uid），带三个档位
        // （变体的档位是 medium / high / max —— 顺序按 LEVEL_RANK）
        let swe = listed_models.iter().find(|m| m.id == "swe-2").expect("swe-2 一条");
        assert_eq!(vec!["medium", "high", "max"], swe.efforts);
        // gpt-5.6-sol 有 priority 快速档 → 多出一条 gpt-5.6-sol-priority
        assert!(
            listed_models.iter().any(|m| m.id == "gpt-5.6-sol-priority"),
            "有 priority 变体的家族要额外出一条快速档"
        );
        // 家族 id + 档位 → 最近的那条变体
        assert_eq!("swe-2-medium", variant_for(&families, "swe-2", "medium"));
        assert_eq!("swe-2-max", variant_for(&families, "swe-2", "max"));
        // 请求 high：swe-2 恰好有 high 那条，发它
        assert_eq!("swe-2-high", variant_for(&families, "swe-2", "high"));
        // 没请求档位 → 家族默认（swe-2 第一条）
        assert_eq!("swe-2-high", variant_for(&families, "swe-2", ""));
        // 别名也能点到
        assert_eq!("claude-sonnet-5-low", variant_for(&families, "claude", "low"));
        // 直接点变体 id 原样发出
        assert_eq!("swe-2-medium", variant_for(&families, "swe-2-medium", ""));
    }

    #[test]
    fn effort_of_maps_ultra_and_rejects_unknowns() {
        assert_eq!("max", effort_of("ultra"));
        assert_eq!("low", effort_of("LOW"));
        assert_eq!("", effort_of("turbo"));
    }

    #[test]
    fn a_single_variant_family_is_offered_under_its_own_id() {
        // Claude Haiku 4.5 只有一个变体 → 家族条目照出（id 是家族 uid）
        let families = snapshot_families();
        let images = snapshot_images();
        let listed_models = listed(&families, &[], &images, &HashSet::new(), &HashMap::new());
        assert!(listed_models.iter().any(|m| m.id == "Claude Haiku 4.5"));
    }
}
