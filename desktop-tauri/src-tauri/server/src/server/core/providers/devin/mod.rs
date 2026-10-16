//! Devin 提供商：把 Devin 订阅（devin CLI 那个账号）**反转成 OpenAI 兼容 API**。
//!
//! ── 这一家在网关眼里长什么样 ──────────────────────────────────
//! 上游不是 OpenAI 协议，而是 Windsurf 的 **Connect-RPC `GetChatMessage`**
//! （protobuf 载荷 + Connect 帧，鉴权是 `Authorization: Basic <token>`）。
//! 一次「聊天补全」到这里要被**整段自己拼**：把 OpenAI 的消息列表折成 Devin 的
//! `Message` 序列（系统提示词塞进第一条 user、工具说明进 `<tool_descriptions>`、
//! schema 折成根对象、Gemini 系再过一道），再把回来的 protobuf 帧翻成
//! `chat.completion.chunk`。单请求构造容纳不了这套「构造 + 发送 + 翻译」三步，
//! 所以本家像 CodeArts / Trae / Qoder / Accio 一样走**会话式转发**
//! （`is_stateful() == true`），由 `forward_conversation` 自己完成发送与翻译 ——
//! 编排层的账号轮换、限额冷却、记账、取消处理因此零改动。
//!
//! ── 移植来源 ────────────────────────────────────────────────
//! 逐条对齐 MIT 许可的参考实现 magpie-community/plugins 的
//! `packages/devin/index.mjs`。字段号、常量、shaping 规则都取自它，不另行发明。
//!
//! ── 模块分工 ────────────────────────────────────────────────
//!   endpoints.rs    端点常量与基址覆盖
//!   pb.rs           protobuf 手写编解码 + Connect 帧
//!   credentials.rs  凭证（会话令牌 + 只读导入 CLI 的 credentials.toml）
//!   oauth.rs        PKCE 授权与换码（网页登录）+ 令牌身份确认
//!   models.rs       快照表 + GetCliModelConfigs 解析 + 清单折叠 + 变体选择
//!   chat.rs         GetChatMessage 请求构造（消息 shaping / schema 折叠）
//!   stream.rs       回复帧翻译 + 错误分类 + SSE 组装

pub mod chat;
pub mod credentials;
pub mod endpoints;
pub mod models;
pub mod oauth;
pub mod pb;
pub mod stream;

use std::pin::Pin;

use axum::http::HeaderMap;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::server::core::account_store::AccountStore;
use crate::server::core::egress;
use crate::server::core::upstream::usage::RequestTelemetry;
use crate::server::core::upstream::ForwardOutcome;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::adapter::{ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass};
use super::ProviderKind;

/// Devin 适配器（无状态字段：目录缓存在 `models` 里，凭据在账号存储里）。
pub struct DevinAdapter;

/// 静态实例：`adapter_for` 要的是 `&'static dyn ProviderAdapter`。
pub static DEVIN_ADAPTER: DevinAdapter = DevinAdapter;

impl ProviderAdapter for DevinAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Devin
    }

    /// 清单来自刷新链路落的进程内缓存（`list_models` 是同步契约，发不了网络请求）；
    /// 还没刷过时用内置快照把清单先立起来。
    fn list_models(&self) -> Vec<Value> {
        models::list_values()
    }

    /// 防御性报错：Devin 的对话要先折消息、再包 Connect 帧、再翻回复流，单请求
    /// 构造路径容纳不了 —— 与 CodeArts / Qoder / Accio 同一处境。
    fn build_chat_request(
        &self,
        _account: &Value,
        _body: &Value,
        _client_headers: &HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        Err(GatewayError::with_status(
            503,
            "Devin 走会话式转发，不走单请求路径（内部错误：编排层未按 is_stateful 分流）",
        ))
    }

    fn is_stateful(&self) -> bool {
        true
    }

    fn supports_chat(&self) -> bool {
        true
    }

    /// 错误分类（无状态路径不会走到，但有状态路径之外的调用点仍会问它）。
    /// 判据复用 `stream::classify_failure`，再按 HTTP 语义分档。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        let error = classify_from_body(status, error_body);
        match error.status_code {
            429 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: error.message,
                upstream_code: error.upstream_code,
                status: 429,
            },
            _ => UpstreamErrorClass::Fatal {
                status: u16::try_from(error.status_code).unwrap_or(status),
                message: error.message,
                upstream_code: error.upstream_code,
            },
        }
    }

    /// 会话式路径的分类：**429 落冷却**（额度 / 限流，换下一个账号），
    /// 内容策略拦截交 `ContentBlocked`（不罚账号，换中性提示词重试），
    /// 其余（含 401 —— 本家没有续期链，重试同一枚令牌只会再失败一次）透传。
    fn classify_conversation_error(&self, error: &GatewayError) -> UpstreamErrorClass {
        match error.status_code {
            429 => UpstreamErrorClass::QuotaLimited {
                reset_at: None,
                message: error.message.clone(),
                upstream_code: error.upstream_code,
                status: 429,
            },
            400 if error.message.to_lowercase().contains("content policy") => {
                UpstreamErrorClass::ContentBlocked {
                    status: 400,
                    message: error.message.clone(),
                    upstream_code: error.upstream_code,
                }
            }
            _ => UpstreamErrorClass::Fatal {
                status: u16::try_from(error.status_code).unwrap_or(500),
                message: error.message.clone(),
                upstream_code: error.upstream_code,
            },
        }
    }

    /// 没有续期（上游没有刷新接口）：令牌被拒只能重新登录，所以 `supports_refresh`
    /// 保持默认 false，不再是「临期」这一说。
    fn supports_refresh(&self) -> bool {
        false
    }

    /// 取一枚可用的会话令牌（本家没有刷新，未指定账号时取队首；取不到就报错）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move {
            let (credential, _) = account_credential(store, account_id)?;
            Ok(credential.token)
        })
    }

    /// 目录**能远程刷**（`GetCliModelConfigs`），必须显式打开：漏了它刷新循环
    /// 永远不会问本家，界面还会反过来报「该提供商使用固定模型清单」。
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 目录刷新：`GetCliModelConfigs` → 解析成家族 + 图片 + 免费 + 标价 → 落缓存。
    ///
    /// 「没有账号」不是失败（自动路径每轮都会走到这里）：与 accio / codearts 同口径，
    /// 自动路径 `unchanged()`，点名取不到才 `failed()`。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        _force: bool,
    ) -> Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>> {
        Box::pin(async move {
            let Some(record) = store.devin_account_record(account_id) else {
                logging::verbose("[Models]", "Devin 模型目录刷新跳过：尚未添加 Devin 账号");
                if account_id.trim().is_empty() {
                    return ModelRefreshOutcome::unchanged();
                }
                return ModelRefreshOutcome::failed("指定的 Devin 账号不存在或不可用，请重新选择");
            };
            let credential = match credentials::Credential::from_payload(&record) {
                Ok(credential) => credential,
                Err(reason) => return ModelRefreshOutcome::failed(reason),
            };
            let proxy = record_proxy(store, account_id);
            let base = credential.server();
            let client = egress::client_for(proxy.as_ref());
            let mut body = pb::Pb::new();
            body.bytes(1, &chat::metadata(&credential.token));
            let response = match client
                .post(endpoints::model_configs_url(&base))
                .header("Content-Type", "application/proto")
                .header("Connect-Protocol-Version", "1")
                .timeout(std::time::Duration::from_secs(15))
                .body(body.done())
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    return ModelRefreshOutcome::failed(format!(
                        "Devin 目录请求失败：{}",
                        egress::describe_error_detail(&error)
                    ))
                }
            };
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                let text = response.text().await.unwrap_or_default();
                let detail: String = text.chars().take(200).collect();
                return ModelRefreshOutcome::failed(format!("Devin 目录接口返回 {status}: {detail}"));
            }
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return ModelRefreshOutcome::failed(format!(
                        "读取 Devin 目录响应失败：{}",
                        egress::describe_error_detail(&error)
                    ))
                }
            };
            let Some(images) = models::parse_model_configs(&bytes) else {
                return ModelRefreshOutcome::failed("该账号没有返回任何可用模型");
            };
            let families = models::parse_config_families(&bytes);
            if families.is_empty() {
                return ModelRefreshOutcome::failed("该账号没有返回任何可用模型");
            }
            let free = models::parse_free(&bytes);
            let prices = models::parse_prices(&bytes);
            models::store_catalog(models::Catalog {
                families,
                images,
                free,
                prices,
                ..Default::default()
            });
            let count = models::list_values().len();
            logging::log("[Models]", &format!("✅ Devin 模型目录已刷新（{count} 个）"));
            ModelRefreshOutcome::refreshed(count)
        })
    }

    /// 网页登录：PKCE 授权页 + 回调里的 `code` / `state`（与 Accio / Antigravity 同款）。
    fn supports_web_login(&self) -> bool {
        true
    }

    /// 授权地址由本进程现造（PKCE verifier 记在 `oauth` 的待办表里）。
    fn build_login_url(&self) -> Option<(String, String)> {
        Some(oauth::begin_login())
    }

    /// 用回调里的一次性 `code` 换凭证、认身份、落账号。
    fn exchange_login_code<'a>(
        &'a self,
        store: &'a AccountStore,
        code: &'a str,
        state: &'a str,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>> {
        Box::pin(async move { oauth::exchange_and_store(store, code, state).await })
    }

    /// **会话式转发入口**：构造请求 → 发包 → 翻回复（流式逐帧下发 / 非流式聚合）。
    fn forward_conversation<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        body: &'a Value,
        _client_headers: &'a HeaderMap,
        proxy: Option<crate::server::core::proxies::ResolvedProxy>,
        stream: bool,
        telemetry: &'a std::sync::Arc<RequestTelemetry>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ForwardOutcome, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            // ① 凭据：令牌 + 应用服务器基址（含按账号覆盖）
            let (credential, account_proxy) = account_credential(store, account_id)?;
            let effective_proxy = proxy.or(account_proxy);

            // ② 请求的模型名与档位 → 真正发给上游的变体 id
            let model = body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if model.is_empty() {
                return Err(GatewayError::with_status(
                    400,
                    "Devin 请求缺少 model（请在请求体里指定一个模型名）",
                ));
            }
            let families = models::current_families();
            let effort = client_effort(body);
            let uid = models::variant_for(&families, &model, &effort);

            // ③ 构造并发送（Connect 帧 + Basic 鉴权）
            let payload = pb::frame(&chat::build_get_chat_message(body, &uid, &credential.token));
            let base = credential.server();
            let client = egress::client_for(effective_proxy.as_ref());
            let budget = std::time::Duration::from_millis(
                crate::server::config::timeout_settings().headers_ms(),
            );
            logging::verbose(
                "[Devin]",
                &format!(
                    "POST {} model={} upstream={} stream={} account={}",
                    endpoints::chat_url(&base),
                    model,
                    uid,
                    stream,
                    if account_id.is_empty() { "(队首)" } else { account_id },
                ),
            );
            let response = match tokio::time::timeout(
                budget,
                client
                    .post(endpoints::chat_url(&base))
                    .header("Content-Type", "application/connect+proto")
                    .header("Connect-Protocol-Version", "1")
                    .header("Authorization", format!("Basic {}", credential.token))
                    .body(payload)
                    .send(),
            )
            .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    return Err(GatewayError::with_status(
                        502,
                        format!("Devin 对话请求失败：{}", egress::describe_error_detail(&error)),
                    ))
                }
                Err(_elapsed) => {
                    return Err(GatewayError::with_status(
                        502,
                        format!("Devin 等待响应超时({}秒)", budget.as_secs()),
                    ))
                }
            };
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                let text = response.text().await.unwrap_or_default();
                let detail: String = text.chars().take(1024 * 1024).collect();
                return Err(stream::classify_failure(status, &detail));
            }

            // ④ 首帧预读：一个字节都没下发之前就把「流内的错误」判出来，
            //    编排层据此换账号（此刻还没下发任何字节，换号无损）。
            let mut decoder = pb::FrameDecoder::new();
            let mut source = response.bytes_stream();
            let mut prefetched: Vec<pb::Frame> = Vec::new();
            while let Some(item) = source.next().await {
                match item {
                    Ok(chunk) => {
                        let frames = decoder
                            .push(&chunk)
                            .map_err(|error| GatewayError::with_status(502, error))?;
                        for frame in frames {
                            if frame.end {
                                if let Some(error) = stream::end_frame_error(&frame) {
                                    return Err(error);
                                }
                                return Err(GatewayError::with_status(502, "Devin 上游回了空回答"));
                            }
                            prefetched.push(frame);
                        }
                        if !prefetched.is_empty() {
                            break;
                        }
                    }
                    Err(error) => {
                        return Err(GatewayError::with_status(
                            502,
                            format!(
                                "Devin 上游流中断：{}",
                                egress::describe_error_detail(&error)
                            ),
                        ))
                    }
                }
            }
            if prefetched.is_empty() {
                // 流在没有任何内容帧之前就结束了
                return Err(GatewayError::with_status(502, "Devin 上游回了空回答"));
            }

            if !stream {
                return aggregate(source, decoder, prefetched, &model, telemetry).await;
            }

            // ⑤ 流式：逐帧翻译并下发（在 spawn 的后台任务里跑）
            let (sender, receiver) =
                tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
            let sniff_telemetry = telemetry.clone();
            let model_name = model.clone();
            crate::spawn_task(async move {
                let mut writer = stream::ChunkWriter::new(&model_name);
                let head = writer.start();
                if sender
                    .send(Ok(bytes::Bytes::from(head)))
                    .await
                    .is_err()
                {
                    return;
                }
                let mut reader = stream::ReplyReader::new();
                for frame in prefetched {
                    for event in reader.consume(&frame.data) {
                        let text = event_frame(&writer, event);
                        if !text.is_empty()
                            && sender.send(Ok(bytes::Bytes::from(text))).await.is_err()
                        {
                            return;
                        }
                    }
                }
                drive_stream(&mut source, &mut decoder, &mut reader, &mut writer, &sender, &sniff_telemetry).await;
                let usage = reader.usage_json();
                sniff_telemetry.report_usage(&usage);
                let tail = writer.finish(reader.finish_reason(), Some(usage));
                let _ = sender.send(Ok(bytes::Bytes::from(tail))).await;
            });
            Ok(ForwardOutcome::Stream {
                status: 200,
                stream: Box::new(tokio_stream::wrappers::ReceiverStream::new(receiver)),
            })
        })
    }
}

/// 后台流式循环：继续吃字节流、把帧翻成下发分片。
async fn drive_stream<S>(
    source: &mut S,
    decoder: &mut pb::FrameDecoder,
    reader: &mut stream::ReplyReader,
    writer: &mut stream::ChunkWriter,
    sender: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    telemetry: &std::sync::Arc<RequestTelemetry>,
) where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    while let Some(item) = source.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => {
                let message = format!("Devin 上游流中断：{}", egress::describe_error_detail(&error));
                telemetry.note_error(&message);
                logging::verbose("[Devin]", &message);
                break;
            }
        };
        let frames = match decoder.push(&chunk) {
            Ok(frames) => frames,
            Err(error) => {
                telemetry.note_error(&error);
                logging::verbose("[Devin]", &format!("帧解码失败：{error}"));
                break;
            }
        };
        let mut stop = false;
        for frame in frames {
            if frame.end {
                if let Some(error) = stream::end_frame_error(&frame) {
                    telemetry.note_error(&error.message);
                    logging::verbose("[Devin]", &format!("流内错误：{}", error.message));
                }
                stop = true;
                break;
            }
            for event in reader.consume(&frame.data) {
                let text = event_frame(writer, event);
                if !text.is_empty() && sender.send(Ok(bytes::Bytes::from(text))).await.is_err() {
                    return;
                }
            }
        }
        if stop {
            break;
        }
    }
}

/// 一条增量事件 → 下发分片文本（空文本表示这条不下发）。
fn event_frame(writer: &stream::ChunkWriter, event: stream::ReplyEvent) -> String {
    match event {
        stream::ReplyEvent::Text(text) => writer_delta(writer, json!({ "content": text })),
        stream::ReplyEvent::Reasoning(text) => {
            writer_delta(writer, json!({ "reasoning_content": text }))
        }
        stream::ReplyEvent::ToolCall { index, id, name } => writer_delta(
            writer,
            json!({ "tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": "" },
            }] }),
        ),
        stream::ReplyEvent::ToolArgs { index, text } => writer_delta(
            writer,
            json!({ "tool_calls": [{ "index": index, "function": { "arguments": text } }] }),
        ),
    }
}

/// `ChunkWriter::delta` 需要 `&mut`，这里把「按事件构 delta」与「追加首帧」收一处。
fn writer_delta(writer: &stream::ChunkWriter, delta: Value) -> String {
    // `ChunkWriter` 的分片组装是纯拼接；这里用 `&ChunkWriter` 直接发（首帧由调用方先发过）
    stream::sse_frame(&json!({
        "id": writer.id(),
        "object": "chat.completion.chunk",
        "created": writer.created(),
        "model": writer.model(),
        "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }],
    }))
}

/// 非流式：把剩下的帧全读完，聚合成一个 `chat.completion`。
async fn aggregate<S>(
    mut source: S,
    mut decoder: pb::FrameDecoder,
    prefetched: Vec<pb::Frame>,
    model: &str,
    telemetry: &std::sync::Arc<RequestTelemetry>,
) -> Result<ForwardOutcome, GatewayError>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let mut reader = stream::ReplyReader::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<Value> = Vec::new();
    let mut frames = prefetched;
    loop {
        for frame in frames.drain(..) {
            if frame.end {
                if let Some(error) = stream::end_frame_error(&frame) {
                    return Err(error);
                }
                return Ok(completion_outcome(
                    model, &content, &reasoning, calls, reader, telemetry,
                ));
            }
            apply_event(reader.consume(&frame.data), &mut content, &mut reasoning, &mut calls);
        }
        let Some(item) = source.next().await else {
            break;
        };
        let chunk = item.map_err(|error| {
            GatewayError::with_status(
                502,
                format!("Devin 上游流中断：{}", egress::describe_error_detail(&error)),
            )
        })?;
        frames = decoder
            .push(&chunk)
            .map_err(|error| GatewayError::with_status(502, error))?;
    }
    Ok(completion_outcome(
        model, &content, &reasoning, calls, reader, telemetry,
    ))
}

/// 把一批增量事件累进内容 / 思考 / 工具调用累加器。
fn apply_event(
    events: Vec<stream::ReplyEvent>,
    content: &mut String,
    reasoning: &mut String,
    calls: &mut Vec<Value>,
) {
    for event in events {
        match event {
            stream::ReplyEvent::Text(text) => content.push_str(&text),
            stream::ReplyEvent::Reasoning(text) => reasoning.push_str(&text),
            stream::ReplyEvent::ToolCall { id, name, .. } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": "" },
            })),
            stream::ReplyEvent::ToolArgs { index, text } => {
                if let Some(call) = calls.get_mut(index) {
                    if let Some(arguments) = call
                        .get_mut("function")
                        .and_then(|function| function.get_mut("arguments"))
                    {
                        if let Some(existing) = arguments.as_str() {
                            *arguments = Value::String(format!("{existing}{text}"));
                        }
                    }
                }
            }
        }
    }
}

/// 聚合产出（`chat.completion`）。
fn completion_outcome(
    model: &str,
    content: &str,
    reasoning: &str,
    calls: Vec<Value>,
    reader: stream::ReplyReader,
    telemetry: &std::sync::Arc<RequestTelemetry>,
) -> ForwardOutcome {
    let usage = reader.usage_json();
    telemetry.report_usage(&usage);
    let writer = stream::ChunkWriter::new(model);
    ForwardOutcome::Completion {
        body: stream::completion_body(
            &writer,
            content,
            reasoning,
            calls,
            reader.finish_reason(),
            Some(usage),
        ),
    }
}

/// 从账号记录读凭据与它自己的代理（没有可用账号 → 503）。
fn account_credential(
    store: &AccountStore,
    account_id: &str,
) -> Result<(credentials::Credential, Option<crate::server::core::proxies::ResolvedProxy>), GatewayError> {
    let record = store.devin_account_record(account_id).ok_or_else(|| {
        GatewayError::with_status(503, "没有可用的 Devin 账号：请在账号页添加并启用账号")
    })?;
    let credential = credentials::Credential::from_payload(&record)
        .map_err(|reason| GatewayError::with_status(503, reason))?;
    Ok((credential, record_proxy(store, account_id)))
}

/// 解析账号自己配的代理（解析失败不静默降级成直连）。
fn record_proxy(
    store: &AccountStore,
    account_id: &str,
) -> Option<crate::server::core::proxies::ResolvedProxy> {
    let record = store.devin_account_record(account_id)?;
    use crate::server::core::proxies::ProxyResolution;
    match crate::server::core::proxies::resolve_account_proxy(record.get("proxy")) {
        Some(ProxyResolution::Resolved(proxy)) => Some(proxy),
        _ => None,
    }
}

/// 客户端请求体里显式指定的思考档位（`reasoning_effort` 等通用键）。
fn client_effort(body: &Value) -> String {
    for key in ["reasoning_effort", "reasoningEffort", "effort", "reasoning"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            let effort = models::effort_of(value);
            if !effort.is_empty() {
                return effort;
            }
        }
    }
    String::new()
}

/// 把归一化的错误体（含 code / message）合成一次 `classify_failure`。
fn classify_from_body(status: u16, error_body: &Value) -> GatewayError {
    let code = error_body.get("code").and_then(Value::as_str).unwrap_or("");
    let message = error_body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let text = json!({ "code": code, "message": message }).to_string();
    stream::classify_failure(status, &text)
}
