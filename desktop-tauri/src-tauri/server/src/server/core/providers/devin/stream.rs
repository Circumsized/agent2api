//! GetChatMessage 的**回复流**：把 Connect 帧解成 chat 完成分片，以及错误分类。
//!
//! ── 回复帧长什么样 ────────────────────────────────────────────
//! 上游回的是一串 Connect 帧（见 `pb.rs`）。普通帧的载荷是一条
//! `GetChatMessageResponse`，里面按字段号分段吐内容：
//!   `3` 正文增量、`9` 思考增量、`6` 一条工具调用的片段（内层 1=id、2=name、
//!   3=参数文本）、`5` 停止原因、`7` 用量（内层 2=输入、3=输出、4=写缓存、
//!   5=读缓存）。最后一帧（`end`）的载荷是一段 JSON：成功是空对象，失败是
//!   `{"error":{"code","message"}}` 或 `{"code","message"}`。
//!
//! ── 错误分类为什么在这里 ─────────────────────────────────────
//! Devin 的错误码是 Connect 那一套（`unauthenticated` / `resource_exhausted` /
//! `invalid_argument` / `unavailable`…），要翻成 HTTP 语义与用户能读的中文。
//! 参考实现的 `failure()` 逐条做了这件事，这里照搬；产出的 [`GatewayError`]
//! 就是全仓统一的错误类型，编排层（换号 / 冷却 / 透传）不用认识 Devin 的码。

use serde_json::{json, Value};

use crate::server::errors::GatewayError;

use super::pb::{self, Frame};

/// 一次回复累计的用量
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

/// 回复流里的一条增量事件（与参考实现 `events()` 逐条对应）。
#[derive(Clone, Debug)]
pub enum ReplyEvent {
    /// 正文增量
    Text(String),
    /// 思考增量（下发为 `reasoning_content`）
    Reasoning(String),
    /// 开一条工具调用（`index` 从 0 起）
    ToolCall { index: usize, id: String, name: String },
    /// 某条工具调用的参数增量
    ToolArgs { index: usize, text: String },
}

/// 把一帧回复的载荷读成事件，并累计「当前这条回复」的停止原因与用量。
#[derive(Default)]
pub struct ReplyReader {
    next_index: i64,
    stop: u64,
    said: usize,
    usage: Usage,
}

impl ReplyReader {
    pub fn new() -> Self {
        Self {
            next_index: -1,
            stop: 0,
            said: 0,
            usage: Usage::default(),
        }
    }

    /// 解析一帧载荷，返回其中的事件。
    pub fn consume(&mut self, payload: &[u8]) -> Vec<ReplyEvent> {
        let mut events = Vec::new();
        for field in pb::fields(payload) {
            if field.num == 3 && field.wire == 2 {
                if field.data.is_empty() {
                    continue;
                }
                self.said += field.data.len();
                events.push(ReplyEvent::Text(
                    String::from_utf8_lossy(&field.data).to_string(),
                ));
            } else if field.num == 9 && field.wire == 2 {
                if field.data.is_empty() {
                    continue;
                }
                self.said += field.data.len();
                events.push(ReplyEvent::Reasoning(
                    String::from_utf8_lossy(&field.data).to_string(),
                ));
            } else if field.num == 6 && field.wire == 2 {
                let mut id = String::new();
                let mut name = String::new();
                let mut args = String::new();
                for inner in pb::fields(&field.data) {
                    match inner.num {
                        1 => id = String::from_utf8_lossy(&inner.data).to_string(),
                        2 => name = String::from_utf8_lossy(&inner.data).to_string(),
                        3 => args.push_str(&String::from_utf8_lossy(&inner.data)),
                        _ => {}
                    }
                }
                if !name.is_empty() {
                    self.next_index += 1;
                    let id = if id.is_empty() {
                        format!("call_{}", random_hex(12))
                    } else {
                        super::chat::out_id(&id)
                    };
                    events.push(ReplyEvent::ToolCall {
                        index: self.next_index as usize,
                        id,
                        name,
                    });
                }
                if !args.is_empty() && self.next_index >= 0 {
                    events.push(ReplyEvent::ToolArgs {
                        index: self.next_index as usize,
                        text: args,
                    });
                }
            } else if field.num == 5 && field.wire == 0 {
                self.stop = field.n;
            } else if field.num == 7 && field.wire == 2 {
                for inner in pb::fields(&field.data) {
                    if inner.wire != 0 || inner.n == 0 {
                        continue;
                    }
                    match inner.num {
                        2 => self.usage.input = inner.n,
                        3 => self.usage.output = inner.n,
                        4 => self.usage.cache_write = inner.n,
                        5 => self.usage.cache_read = inner.n,
                        _ => {}
                    }
                }
            }
        }
        events
    }

    /// 收尾的 `finish_reason`（有工具调用 / 停止码 10 → `tool_calls`；3 → `length`）。
    pub fn finish_reason(&self) -> &'static str {
        if self.next_index >= 0 || self.stop == 10 {
            "tool_calls"
        } else if self.stop == 3 {
            "length"
        } else {
            "stop"
        }
    }

    /// 归一后的用量：输出缺失时按「正文长度 / 4」估一个。
    pub fn usage_json(&self) -> Value {
        let mut usage = self.usage;
        if usage.output == 0 {
            usage.output = ((self.said + 3) / 4) as u64;
        }
        let prompt = usage.input + usage.cache_read + usage.cache_write;
        json!({
            "prompt_tokens": prompt,
            "completion_tokens": usage.output,
            "total_tokens": prompt + usage.output,
            "prompt_tokens_details": {
                "cached_tokens": usage.cache_read,
                "cache_write_tokens": usage.cache_write,
            },
        })
    }
}

/// 一个 end 帧的载荷 → 错误。成功（空对象）时返回 `None`。
pub fn end_frame_error(frame: &Frame) -> Option<GatewayError> {
    if !frame.end {
        return None;
    }
    let text = String::from_utf8_lossy(&frame.data).to_string();
    let error = classify_failure(200, &text);
    // `classify_failure(200, "{}")` 会落回 200 + 一句 HTTP 文案；那其实是成功。
    if error.status_code < 300 {
        None
    } else {
        Some(error)
    }
}

/// Connect 错误码 / 文案 → 统一网关错误（参考实现 `failure()` 的移植）。
///
/// 判据顺序照搬：`unauthenticated` → 401；额度 / 限流 → 429；内容策略 → 400；
/// 上下文过长 → 400；`invalid_argument` → 400；`unavailable` → 503；其余有码 → 502；
/// 没码就原样用 HTTP 状态码。
pub fn classify_failure(status: u16, text: &str) -> GatewayError {
    let mut message = text.trim().to_string();
    let mut code = String::new();
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        if let Some(error) = value.get("error").filter(|value| value.is_object()) {
            code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        } else if let Some(entry_code) = value.get("code").and_then(Value::as_str) {
            code = entry_code.to_string();
            message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = status_text(status);
    }
    let low = message.to_lowercase();
    let (final_status, final_message) = if code == "unauthenticated" {
        (401, format!("{message} —— 请重新登录 Devin"))
    } else if code == "resource_exhausted" || low.contains("quota") || low.contains("rate limit") {
        (429, format!("用量已达上限: {message}"))
    } else if low.contains("content policy") {
        (400, message)
    } else if low.contains("too long")
        || low.contains("context length")
        || low.contains("too many tokens")
    {
        (400, format!("输入超出模型上下文: {message}"))
    } else if code == "invalid_argument" {
        (400, message)
    } else if code == "unavailable" {
        (503, message)
    } else if !code.is_empty() {
        (502, message)
    } else {
        (status, message)
    };
    GatewayError::with_status(i32::from(final_status), final_message)
}

/// 常见 HTTP 状态的英文短语（参考实现用的是 Node 的 STATUS_CODES）。
fn status_text(status: u16) -> String {
    let text = match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Request Entity Too Large",
        414 => "Request URI Too Long",
        416 => "Requested Range Not Satisfiable",
        418 => "I'm a teapot",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => return format!("HTTP {status}"),
    };
    text.to_string()
}

/// 一个 SSE 帧（`data: {json}\n\n`）
pub fn sse_frame(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// SSE 的结束标记
pub fn sse_done() -> String {
    "data: [DONE]\n\n".to_string()
}

/// 流式下发的分片组装器（`id` / `created` / `model` 一旦定下就复用）。
pub struct ChunkWriter {
    id: String,
    created: i64,
    model: String,
    began: bool,
}

impl ChunkWriter {
    pub fn new(model: &str) -> Self {
        Self {
            id: response_id(),
            created: crate::server::logging::now_ms() / 1000,
            model: model.to_string(),
            began: false,
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn created(&self) -> i64 {
        self.created
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    fn value(&self, delta: Value, finish: Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        })
    }

    /// 首帧（`role: assistant`）；已发过就返回空串。
    pub fn start(&mut self) -> String {
        if self.began {
            return String::new();
        }
        self.began = true;
        sse_frame(&self.value(json!({ "role": "assistant", "content": "" }), Value::Null))
    }

    /// 一个内容增量帧（必要时自动补首帧）。
    pub fn delta(&mut self, delta: Value) -> String {
        let head = self.start();
        format!("{head}{}", sse_frame(&self.value(delta, Value::Null)))
    }

    /// 收尾帧：finish_reason + 可选用量 + `[DONE]`。
    pub fn finish(&mut self, reason: &str, usage: Option<Value>) -> String {
        let head = self.start();
        let mut out = head;
        out.push_str(&sse_frame(&self.value(
            json!({}),
            Value::String(reason.to_string()),
        )));
        if let Some(usage) = usage {
            out.push_str(&sse_frame(&json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": usage,
            })));
        }
        out.push_str(&sse_done());
        out
    }
}

/// 非流式的完整回答（`chat.completion`）。
pub fn completion_body(
    writer: &ChunkWriter,
    content: &str,
    reasoning: &str,
    tool_calls: Vec<Value>,
    finish_reason: &str,
    usage: Option<Value>,
) -> Value {
    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert(
        "content".to_string(),
        if content.is_empty() {
            Value::Null
        } else {
            Value::String(content.to_string())
        },
    );
    if !reasoning.is_empty() {
        message.insert(
            "reasoning_content".to_string(),
            Value::String(reasoning.to_string()),
        );
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    let mut body = json!({
        "id": writer.id(),
        "object": "chat.completion",
        "created": writer.created(),
        "model": writer.model(),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
    });
    if let Some(usage) = usage {
        body["usage"] = usage;
    }
    body
}

/// 响应 id（与别家同形：`chatcmpl-` + 24 位十六进制）
pub fn response_id() -> String {
    let uuid = crate::server::core::upstream::request::new_request_id().replace('-', "");
    let short: String = uuid.chars().take(24).collect();
    format!("chatcmpl-{short}")
}

/// `n` 字节的随机十六进制串（工具调用缺 id 时的兜底）
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let _ = getrandom::getrandom(&mut buf);
    buf.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    //! 用手搭的回复帧喂进 `ReplyReader`，逐事件与用量对；错误分类逐条对。
    use super::*;
    use crate::server::core::providers::devin::pb::Pb;

    #[test]
    fn replies_are_read_into_text_reasoning_and_tool_events() {
        let mut payload = Pb::new();
        payload.bytes(3, b"hello ");
        payload.bytes(9, b"thinking");
        let mut call = Pb::new();
        call.str(1, "call:0#abc").str(2, "Bash").bytes(3, b"{\"cmd\":");
        payload.bytes(6, &call.done());
        let mut call2 = Pb::new();
        call2.bytes(3, b"\"ls\"}");
        payload.bytes(6, &call2.done());
        payload.varint(5, 10);
        let mut usage = Pb::new();
        usage.varint(2, 100).varint(3, 42).varint(4, 5).varint(5, 7);
        payload.bytes(7, &usage.done());

        let mut reader = ReplyReader::new();
        let events = reader.consume(&payload.done());
        // 正文、思考、开一条工具调用（id 被改写）、参数两块
        assert!(matches!(&events[0], ReplyEvent::Text(t) if t == "hello "));
        assert!(matches!(&events[1], ReplyEvent::Reasoning(t) if t == "thinking"));
        match &events[2] {
            ReplyEvent::ToolCall { index, id, name } => {
                assert_eq!(0, *index);
                assert!(id.starts_with("dv_"), "不安全的 id 要被改写：{id}");
                assert_eq!("Bash", name);
            }
            other => panic!("期望工具调用，实际 {other:?}"),
        }
        assert!(matches!(&events[3], ReplyEvent::ToolArgs { index: 0, text } if text == "{\"cmd\":"));
        assert!(matches!(&events[4], ReplyEvent::ToolArgs { index: 0, text } if text == "\"ls\"}"));
        assert_eq!("tool_calls", reader.finish_reason(), "停止码 10 = 工具调用");
        let usage = reader.usage_json();
        assert_eq!(112, usage["prompt_tokens"].as_u64().unwrap(), "100 + 读 7 + 写 5");
        assert_eq!(42, usage["completion_tokens"].as_u64().unwrap());
        assert_eq!(154, usage["total_tokens"].as_u64().unwrap());
    }

    #[test]
    fn a_text_only_reply_ends_with_stop() {
        let mut payload = Pb::new();
        payload.bytes(3, b"done");
        payload.varint(5, 3);
        let mut reader = ReplyReader::new();
        reader.consume(&payload.done());
        assert_eq!("length", reader.finish_reason(), "停止码 3 = 长度用尽");
    }

    #[test]
    fn failure_maps_connect_codes_to_http_semantics() {
        let cases = [
            (r#"{"error":{"code":"unauthenticated","message":"bad token"}}"#, 401),
            (r#"{"error":{"code":"resource_exhausted","message":"no quota"}}"#, 429),
            (r#"{"code":"invalid_argument","message":"bad"}"#, 400),
            (r#"{"error":{"code":"unavailable","message":"down"}}"#, 503),
            (r#"{"error":{"code":"internal","message":"boom"}}"#, 502),
        ];
        for (text, expected) in cases {
            let error = classify_failure(200, text);
            assert_eq!(expected, error.status_code, "文案：{text}");
        }
        // 内容策略 / 上下文过长：没有码也映射到 400
        assert_eq!(400, classify_failure(200, "this violates the content policy").status_code);
        assert_eq!(400, classify_failure(200, "the input is too long").status_code);
        assert_eq!(429, classify_failure(200, "rate limit hit").status_code);
        // 没有码、不是 JSON：原样用状态码
        assert_eq!(502, classify_failure(502, "gateway blew up").status_code);
    }

    #[test]
    fn a_successful_end_frame_is_not_an_error() {
        let frame = Frame {
            end: true,
            data: b"{}".to_vec(),
        };
        assert!(end_frame_error(&frame).is_none(), "空对象是成功，不是错误");
        let frame = Frame {
            end: true,
            data: br#"{"error":{"code":"unavailable","message":"down"}}"#.to_vec(),
        };
        let error = end_frame_error(&frame).expect("这是错误");
        assert_eq!(503, error.status_code);
    }

    #[test]
    fn chunk_writer_emits_a_role_frame_then_a_finish_frame() {
        let mut writer = ChunkWriter::new("swe-2");
        let text = writer.delta(json!({ "content": "hi" }));
        assert!(text.contains("\"role\":\"assistant\""), "首个增量要带 role 首帧");
        assert!(text.contains("\"content\":\"hi\""));
        let tail = writer.finish("stop", Some(json!({ "prompt_tokens": 1 })));
        assert!(tail.contains("\"finish_reason\":\"stop\""));
        assert!(tail.contains("\"usage\""));
        assert!(tail.ends_with("data: [DONE]\n\n"));
    }
}
