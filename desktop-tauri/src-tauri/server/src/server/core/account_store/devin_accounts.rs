//! Devin 账号：一枚会话令牌（`devin-session-token$<jwt>`）+ 可选的应用服务器覆盖。
//!
//! ── 记录里为什么有两套键名 ──────────────────────────────────
//! 本家的专有字段（`token` / `api_server_url` / `webapp_host` / `api_url`）**同时要写
//! 通用键名**（`accessToken` / `refreshToken` / `userId` / `expiresAt`）。原因与
//! CodeArts 逐字相同（见 `codearts_accounts.rs` 的模块头）：账号存储的投影
//! （`StoredAccount::has_token` → `access_token()`）读的是 `accessToken`，
//! 不写它 Devin 账号会被当成「没有凭证」而在选路时永远选不中。
//!
//! ── 身份判据：邮箱优先，退到令牌尾号 ────────────────────────
//! 能认出账号是谁（`GetUserStatus` 给了邮箱）时就按邮箱去重；认不出时退到
//! 令牌尾号各占一条 —— 绝不猜着合并（把两条并成一条会让其中一枚令牌被静默丢弃）。
//!
//! ── 没有续期 ────────────────────────────────────────────────
//! 本家上游没有刷新接口，`refreshToken` 恒为空；`update_devin_credentials_if_current`
//! 存在的意义不是「刷新写回」，而是「重新登录 / 换令牌时按身份就地更新」。写回前
//! 仍要比对身份键（记录被换过就判 `Stale`），与另外几家同一纪律。

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::core::providers::devin::credentials::Credential;
use crate::server::core::providers::{kind_id, ProviderKind};
use crate::server::logging;

use super::priority::next_free_priority;
use super::sql;
use super::state::{mark_name_custom, StoredAccount};
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{token_tail_of, truncate_chars};
use super::CredentialWrite;

/// 本家的 provider id（`store_view` 的形状分派与别处比对都要用，与其它几家同一命名）。
pub const DEVIN_PROVIDER_ID: &str = kind_id(ProviderKind::Devin);

const PROVIDER: &str = DEVIN_PROVIDER_ID;

/// 写回前用来比对「还是不是同一枚令牌」的键（变了说明期间被重新添加 / 换过号）。
const IDENTITY_KEYS: &[&str] = &["token", "accessToken", "api_server_url", "userId"];

impl AccountStore {
    /// 取一条 Devin 账号记录；`account_id` 为空时取「启用且能用」的第一条。
    pub fn devin_account_record(&self, account_id: &str) -> Option<Value> {
        let guard = self.guard();
        if !account_id.is_empty() {
            let record = self.record_by_id(&guard, account_id)?;
            return (record.provider() == PROVIDER).then(|| record.to_value());
        }
        self.records_for_provider(&guard, PROVIDER)
            .into_iter()
            .filter(|record| record.enabled() && record.has_token())
            .min_by_key(|record| record.order_key())
            .map(|record| record.to_value())
    }

    /// 添加 / 更新一条 Devin 账号（粘贴令牌、导入 CLI 登录态、网页登录共用这个入口）。
    pub fn add_devin_account(
        &self,
        credential: &Credential,
        name: Option<&str>,
        source: &str,
    ) -> Result<Value, AccountStoreError> {
        if !credential.valid() {
            return Err(AccountStoreError::bad_request("Devin 凭据缺少会话令牌（token）"));
        }
        let guard = self.guard();
        // 身份命中既有记录就地更新（重复登录 / 重新导入不该在列表里堆两条）
        let existing = self
            .records_for_provider(&guard, PROVIDER)
            .into_iter()
            .find(|record| same_identity(record, credential));
        if credential.email.trim().is_empty() {
            // 认不出邮箱时靠令牌尾号去重，只能退到「各占一条」——明确告知原因
            logging::log(
                "[Accounts]",
                "Devin 凭据没带账号邮箱，重复粘贴不同令牌会各占一条账号",
            );
        }
        let id = existing
            .as_ref()
            .map(|record| record.id().to_string())
            .unwrap_or_else(|| {
                let seed = credential.identity();
                format!("devin-{:x}", Sha256::digest(seed.as_bytes()))
            });
        if existing.is_none() && self.record_by_id(&guard, &id).is_some() {
            return Err(AccountStoreError::new(
                "Devin 账号 ID 已被其它账号占用，请先核对账号记录",
                409,
            ));
        }
        let record_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                let email = credential.email.trim();
                (!email.is_empty()).then(|| email.to_string())
            })
            .unwrap_or_else(|| {
                format!("Devin {}", truncate_chars(&credential.token, 8))
            });

        let mut fields = existing
            .as_ref()
            .map(|record| record.fields().clone())
            .unwrap_or_default();
        if let Value::Object(values) = credential.to_record_value() {
            for (key, value) in values {
                // 空值不覆盖既有内容：只重新粘了令牌的添加不该把已有的邮箱 / 服务器
                // 覆盖成空
                if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
                    continue;
                }
                fields.insert(key, value);
            }
        }
        let priority = match existing.as_ref() {
            Some(record) => record.priority(),
            None => {
                let used = self
                    .with_conn(&guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            }
        };
        fields.insert("id".to_string(), Value::String(id.clone()));
        fields.insert("provider".to_string(), Value::String(PROVIDER.to_string()));
        fields.insert("name".to_string(), Value::String(truncate_chars(&record_name, 100)));
        mark_name_custom(
            &mut fields,
            name.is_some_and(|value| !value.trim().is_empty()),
            existing.as_ref(),
        );
        fields.insert(
            "tokenTail".to_string(),
            Value::String(token_tail_of(&credential.token)),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        fields.insert("desktop".to_string(), Value::Bool(false));
        fields.insert("source".to_string(), Value::String(source.to_string()));
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .unwrap_or_else(logging::now_ms),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(logging::now_ms()));
        fields.insert("rateLimits".to_string(), json!({}));
        let record = StoredAccount::from_map(fields);
        self.with_conn(&guard, |conn| sql::put(conn, &record))?;
        logging::log(
            "[Accounts]",
            &format!("✅ Devin 账号已保存（优先级 {priority}）"),
        );
        Ok(self.public_account(&record))
    }

    /// 按身份键比对后再写回（重新登录 / 换令牌时就地更新那条记录）。
    ///
    /// 本家没有续期，这个入口服务的是「身份没变、令牌换了」的那次更新 ——
    /// 比对失败返回 `Stale`，调用方重读而不是覆盖。
    pub fn update_devin_credentials_if_current(
        &self,
        expected: &Value,
        credential: &Credential,
    ) -> Result<CredentialWrite, AccountStoreError> {
        let id = expected.get("id").and_then(Value::as_str).unwrap_or("");
        let guard = self.guard();
        let Some(mut record) = self
            .record_by_id(&guard, id)
            .filter(|record| record.provider() == PROVIDER)
        else {
            return Ok(CredentialWrite::Stale);
        };
        for key in IDENTITY_KEYS {
            if record.get(key) != expected.get(key) {
                return Ok(CredentialWrite::Stale);
            }
        }
        if let Value::Object(values) = credential.to_record_value() {
            for (key, value) in values {
                if value.is_null() || matches!(&value, Value::String(text) if text.is_empty()) {
                    continue;
                }
                record.fields_mut().insert(key, value);
            }
        }
        record.set("tokenTail", Value::String(token_tail_of(&credential.token)));
        record.set_updated_at(logging::now_ms());
        self.with_conn(&guard, |conn| sql::update_in_place(conn, &record))?;
        Ok(CredentialWrite::Written)
    }

    /// 界面用的公开形态。**绝不带令牌本体**（只给尾号与「有没有令牌」）。
    pub fn to_devin_public_account(&self, record: &StoredAccount) -> Value {
        let stored = record.to_value();
        let credential = Credential::from_payload(&stored).ok();
        let available = record.has_token() && credential.as_ref().is_some_and(|value| value.valid());
        let mut public = serde_json::Map::new();
        for key in ["id", "provider", "name", "userId", "source", "tokenTail", "expiresAt"] {
            public.insert(
                key.to_string(),
                record.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        public.insert("edition".to_string(), Value::Null);
        public.insert("editionLabel".to_string(), Value::Null);
        // 本家没有续期：恒为 false（界面据此不渲染「刷新令牌」那一档）
        public.insert("hasRefreshToken".to_string(), Value::Bool(false));
        public.insert("priority".to_string(), Value::from(record.priority()));
        public.insert("enabled".to_string(), Value::Bool(record.enabled()));
        public.insert("addedAt".to_string(), Value::from(record.added_at()));
        public.insert("updatedAt".to_string(), Value::from(record.updated_at()));
        public.insert(
            "proxy".to_string(),
            crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
        );
        public.insert(
            "rateLimits".to_string(),
            record.get("rateLimits").cloned().unwrap_or_else(|| json!({})),
        );
        public.insert("desktop".to_string(), Value::Bool(false));
        public.insert("available".to_string(), Value::Bool(available));
        // 应用服务器覆盖（有才透出；缺省跟随内置基址）
        if let Some(server) = record.get("api_server_url").and_then(Value::as_str) {
            if !server.trim().is_empty() {
                public.insert(
                    "server".to_string(),
                    Value::String(server.trim().to_string()),
                );
            }
        }
        Value::Object(public)
    }
}

/// 记录与本家凭据是不是同一身份（邮箱优先，退到令牌尾号）。
fn same_identity(record: &StoredAccount, credential: &Credential) -> bool {
    let stored = record.to_value();
    let read = |key: &str| {
        stored
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let email = credential.email.trim();
    if !email.is_empty() {
        return read("userId") == email || read("token") == credential.token.trim();
    }
    let tail = token_tail_of(&credential.token);
    !tail.is_empty() && read("tokenTail") == tail
}
