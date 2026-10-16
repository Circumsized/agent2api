//! GetChatMessage 请求的构造（Devin 的对话请求 shaping）。
//!
//! ── 为什么这一家的「请求体构造」要自己写一大段 ────────────────
//! Devin 的上游不是 OpenAI 协议：它收的是一整段**多步会话**的 protobuf
//! （用户 / 助手 / 工具消息各是一条 `Message`），且对「怎么组织这些消息」有一套
//! 自己的脾气（参考实现 `build()` 里的每一条注释都是一次实测）：
//!   * **system / developer 消息会被上游拒**，因此系统提示词与工具说明被拼进
//!     **第一条 user 消息的开头**；
//!   * 工具**不**随各自的 JSON Schema 原样下发，而是把说明集中写进
//!     `<tool_descriptions>` 块、每条工具只带一句「去看那段说明」的指针 ——
//!     Devin 对「随工具带一大段描述」的请求会回「an internal error occurred」；
//!   * Claude 系的模型拒收根上有 `anyOf` / `oneOf` / `allOf` 的工具 schema，
//!     所以要先把 schema 折成「根就是个对象」（[`object_root`]）；
//!   * Gemini 系的模型拒收「类型是数组」等形态的 schema，还要再过一道
//!     [`gemini_schema`]；
//!   * `temperature` / `top_p` 恰好为 0 会让上游回「an internal error」，要抬到
//!     1e-6（[`CHAT_TOP_EPSILON`]）；
//!   * 一次没被回答的工具调用要被补一条「被打断」的工具消息
//!     （[`NO_RESULT`]），否则会话在 Devin 眼里不完整。
//! 这些规则逐字移植参考实现 `packages/devin/index.mjs`（MIT）的 `build()`。

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use super::endpoints::{self, CLI_VERSION};
use super::pb::Pb;

/// 会话里的角色（protobuf 枚举值）
pub const ROLE_USER: u64 = 1;
pub const ROLE_ASSISTANT: u64 = 2;
pub const ROLE_TOOL: u64 = 4;

/// 回答一个「会话没答过」的工具调用时补的文案
pub const NO_RESULT: &str = "Tool use was interrupted and did not produce a result.";

/// temperature / top_p 的最小下界：上游对**恰好 0** 会回内部错误
const CHAT_TOP_EPSILON: f64 = 1e-6;

/// 工具返回图片的「下一封信」写法（见参考实现的 `FROM` / `NOTE`）
const FROM: &str = "[From the result of ";

/// 构造 `GetChatMessage` 的请求体（protobuf 字节，交给 `pb::frame` 包成 Connect 帧）。
///
/// `chat` 是客户端请求体（OpenAI Chat Completions 形态，已经过编排层的映射改写），
/// `uid` 是**真正发给上游的变体 id**（由 `models::variant_for` 选出来），
/// `token` 是会话语令牌（进 metadata 字段 3）。
pub fn build_get_chat_message(chat: &Value, uid: &str, token: &str) -> Vec<u8> {
    let empty = Vec::new();
    let raw_messages = chat
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let messages = join_split_calls(raw_messages);

    let mut msgs: Vec<OutMsg> = Vec::new();
    // 最近一条助手回复发出、还没被回答的工具调用
    let mut pending: Vec<OutCall> = Vec::new();
    let mut system: Vec<String> = Vec::new();
    // 刚发出去的工具结果，按「客户端给的那个 tool_call_id」索引 ——
    // 它们的图片会被随后那条 user 消息带回（见下面的 result_for）
    let mut results: Vec<ResultEntry> = Vec::new();

    for message in &messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" || role == "developer" {
            system.push(text_of(message.get("content")));
            continue;
        }
        if role != "tool" && role != "user" {
            settle_results(&mut msgs, &mut results);
        }
        if role == "assistant" {
            let calls: Vec<OutCall> = message
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|call| OutCall {
                            id: in_id(call.get("id").and_then(Value::as_str).unwrap_or("")),
                            name: call
                                .get("function")
                                .and_then(|function| function.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            args: args_of(
                                call.get("function").and_then(|function| function.get("arguments")),
                            ),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let content = message.get("content");
            let text = match content {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                _ => String::new(),
            };
            // 一轮只有思考 / 失败的助手消息不发给上游
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            answer_pending(&mut msgs, &mut pending);
            msgs.push(OutMsg {
                role: ROLE_ASSISTANT,
                text,
                calls: calls.clone(),
                call_id: String::new(),
                images: Vec::new(),
            });
            pending = calls;
            continue;
        }
        if role == "tool" {
            // 只回答「刚发出的」调用，且只回答一次
            let tool_call_id = in_id(
                message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            if let Some(index) = pending.iter().position(|call| call.id == tool_call_id) {
                let images: Vec<InlineImage> = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| parts.iter().filter_map(inline_image).collect())
                    .unwrap_or_default();
                let text = text_of(message.get("content"));
                let msg_index = msgs.len();
                msgs.push(OutMsg {
                    role: ROLE_TOOL,
                    text: text.clone(),
                    calls: Vec::new(),
                    call_id: pending[index].id.clone(),
                    images,
                });
                let (said_n, said_text) = noted(&text);
                results.push(ResultEntry {
                    msg_index,
                    id: message
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    said_n,
                    said_text,
                    seen: 0,
                });
                pending.remove(index);
            }
            continue;
        }
        if role != "user" {
            continue;
        }
        let mut texts: Vec<String> = Vec::new();
        let mut images: Vec<InlineImage> = Vec::new();
        match message.get("content") {
            Some(Value::String(text)) => {
                if !text.is_empty() {
                    texts.push(text.clone());
                }
            }
            _ => {
                // 工具返回的图片（聊天协议的工具消息装不下）要跟着它的结果回去，
                // 落在 **tool 消息的图片字段** 上，而不是单独发一条 user 图片消息
                let mut to: Option<usize> = None;
                let parts = message
                    .get("content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for part in &parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                            to = if results.is_empty() {
                                None
                            } else {
                                result_for(text, &results)
                            };
                            if to.is_none() && !text.is_empty() {
                                texts.push(text.to_string());
                            }
                        }
                        Some("image_url") => {
                            let image = inline_image(part);
                            if let Some(index) = to {
                                if let Some(image) = image {
                                    msgs[index].images.push(image);
                                }
                                results[index].seen += 1;
                                if results[index].seen == results[index].said_n {
                                    to = None;
                                }
                            } else if let Some(image) = image {
                                images.push(image);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        settle_results(&mut msgs, &mut results);
        let text = texts.join("\n\n");
        if text.is_empty() && images.is_empty() {
            continue;
        }
        answer_pending(&mut msgs, &mut pending);
        msgs.push(OutMsg {
            role: ROLE_USER,
            text,
            calls: Vec::new(),
            call_id: String::new(),
            images,
        });
    }
    settle_results(&mut msgs, &mut results);
    answer_pending(&mut msgs, &mut pending);

    // 回答的格式只能「用话讲」（Devin 没有对应字段），接在系统段末尾
    let format = format_text(chat.get("response_format"));
    if !format.is_empty() {
        system.push(format);
    }
    // 最后一条是「没工具调用的助手回复」时，补一条 user「继续」——
    // 否则上游没有可回答的对象
    let last_is_assistant_without_calls = msgs
        .last()
        .map(|message| message.role == ROLE_ASSISTANT && message.calls.is_empty())
        .unwrap_or(true);
    if last_is_assistant_without_calls {
        msgs.push(OutMsg {
            role: ROLE_USER,
            text: "Please proceed with the task.".to_string(),
            calls: Vec::new(),
            call_id: String::new(),
            images: Vec::new(),
        });
    }

    // 工具：先按「根是对象」折 schema，Gemini 系的模型再过一道 gemini_schema
    let mut tools = chat_tools(chat);
    if chat.get("tool_choice").and_then(Value::as_str) == Some("none") {
        tools.clear();
    }
    if is_gemini(uid) {
        for tool in tools.iter_mut() {
            tool.schema = gemini_schema(&tool.schema, 0);
        }
    }
    // 系统段 + 工具说明：一起塞进第一条 user 消息的开头
    let system_text = system.join("\n\n");
    let instructions = join_non_empty(&[system_text, tool_descriptions(&tools)]);
    if !instructions.is_empty() {
        let at = msgs.iter().position(|message| message.role == ROLE_USER);
        let at = match at {
            Some(at) => at,
            None => {
                msgs.insert(
                    0,
                    OutMsg {
                        role: ROLE_USER,
                        text: String::new(),
                        calls: Vec::new(),
                        call_id: String::new(),
                        images: Vec::new(),
                    },
                );
                0
            }
        };
        let merged = join_non_empty(&[instructions, msgs[at].text.clone()]);
        msgs[at].text = merged;
    }

    let mut out = Pb::new();
    out.bytes(1, &metadata(token));
    for message in &msgs {
        out.bytes(3, &encode_msg(message));
    }
    out.varint(7, 5);
    let max = number_of(chat, "max_completion_tokens")
        .or_else(|| number_of(chat, "max_tokens"))
        .unwrap_or(128_000);
    let temperature = above_zero(number_of_float(chat, "temperature").unwrap_or(1.0));
    let top_p = above_zero(number_of_float(chat, "top_p").unwrap_or(0.95));
    let mut config = Pb::new();
    config
        .varint(1, 1)
        .varint(2, max)
        .varint(3, 400)
        .double(5, temperature)
        .varint(7, 40)
        .double(8, top_p);
    out.bytes(8, &config.done());

    // 客户端给的工具，以及会话里用到、但它已不再提供的工具
    let mut offered: HashSet<String> = HashSet::new();
    for tool in &tools {
        let description = if tool.description.is_empty() {
            String::new()
        } else {
            format!(
                "Described under <tool name=\"{}\"> in <tool_descriptions>, in the instructions.",
                tool.name
            )
        };
        push_tool(&mut out, &mut offered, &tool.name, &description, &tool.schema);
    }
    for message in &msgs {
        for call in &message.calls {
            if !offered.contains(&call.name) {
                push_tool(&mut out, &mut offered, &call.name, "Tool", &Value::Null);
            }
        }
    }
    out.str(21, uid);
    out.done()
}

/// 一条要发出去的会话消息
struct OutMsg {
    role: u64,
    text: String,
    calls: Vec<OutCall>,
    call_id: String,
    images: Vec<InlineImage>,
}

/// 一次工具调用
#[derive(Clone)]
struct OutCall {
    id: String,
    name: String,
    args: String,
}

/// 一张内联图片（Devin 只认 data URL 里的字节 + mime，不收外链）
#[derive(Clone)]
struct InlineImage {
    data: String,
    mime: String,
}

/// 一条刚发出去的工具结果（等它后续的图片）
struct ResultEntry {
    msg_index: usize,
    /// 客户端原始 tool_call_id（不是 out_id 改写后的）
    id: String,
    /// 结果的正文里说有几张图片跟随
    said_n: usize,
    /// 去掉那段说明之后的结果正文
    said_text: String,
    /// 已经随它回来的图片张数
    seen: usize,
}

/// 把「没被回答的工具调用」补成一条被打断的工具消息。
fn answer_pending(msgs: &mut Vec<OutMsg>, pending: &mut Vec<OutCall>) {
    for call in pending.iter() {
        msgs.push(OutMsg {
            role: ROLE_TOOL,
            text: NO_RESULT.to_string(),
            calls: Vec::new(),
            call_id: call.id.clone(),
            images: Vec::new(),
        });
    }
    pending.clear();
}

/// 结算刚发出去的工具结果：图片都回来了就把正文换成「去掉说明」的那份，
/// 一条都没回、正文也空时补 `(no output)`。
fn settle_results(msgs: &mut Vec<OutMsg>, results: &mut Vec<ResultEntry>) {
    for entry in results.iter() {
        if entry.said_n > 0 && entry.seen == entry.said_n {
            msgs[entry.msg_index].text = entry.said_text.clone();
        }
        if msgs[entry.msg_index].text.is_empty() {
            msgs[entry.msg_index].text = "(no output)".to_string();
        }
    }
    results.clear();
}

/// 这条文本是不是「某个刚发出的工具结果的抬头」（是则返回它在 msgs 里的下标）。
fn result_for(text: &str, results: &[ResultEntry]) -> Option<usize> {
    if !text.starts_with(FROM) {
        return None;
    }
    for entry in results {
        if entry.said_n <= entry.seen {
            continue;
        }
        let exact = format!("{FROM}tool call {}:]", entry.id);
        let tail = format!(" (tool call {}):]", entry.id);
        if text == exact || text.ends_with(&tail) {
            return Some(entry.msg_index);
        }
    }
    None
}

/// 结果正文结尾那句「有 N 张图片在下一封信」：返回 `(N, 去掉那句的正文)`。
fn noted(text: &str) -> (usize, String) {
    const MARKER: &str = "[The tool returned ";
    let (suffix, plural) = if text.ends_with("follows in the next message.]") {
        ("follows in the next message.]", false)
    } else if text.ends_with("follow in the next message.]") {
        ("follow in the next message.]", true)
    } else {
        return (0, text.to_string());
    };
    let tail_start = text.len() - suffix.len();
    let Some(start) = text[..tail_start].rfind(MARKER) else {
        return (0, text.to_string());
    };
    let body = &text[start + MARKER.len()..tail_start];
    // 参考实现那边的正则写的是 `... they) follows? in ...`：分组与 `follows?`
    // 之间**留了一个空格**，所以 body 实际是 `"3 images; they "` / `"an image; it "`
    //（tail 是尾随空格）。不 trim 的话下面两条判据恒失败，`noted` 永远返回 0 ——
    // 结果正文里那句注记摘不掉、工具回传的图片也永远回挂不到 tool 消息上，
    // 整条 results / settle_results 链会静默变成死代码。
    let body = body.trim_end();
    let count = if plural {
        let Some(rest) = body.strip_suffix(" images; they") else {
            return (0, text.to_string());
        };
        match rest.parse::<usize>() {
            Ok(value) => value,
            Err(_) => return (0, text.to_string()),
        }
    } else {
        if body != "an image; it" {
            return (0, text.to_string());
        }
        1
    };
    // 切掉标记（含其前可能的 "\n\n"，那是这句匹配的一部分）
    let mut end = start;
    if text[..end].ends_with("\n\n") {
        end -= 2;
    }
    (count, text[..end].to_string())
}

/// `textOf`：字符串内容，或内容数组里 `type == "text"` 的片段拼接。
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .map(|part| part.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// `joinSplitCalls`：把「上一条助手消息已经发过工具调用」的助手消息并进上一条。
///
/// Anthropic 把连续两条助手消息当成同一轮，而一个 agent 可能把一轮里的并行工具调用
/// 拆成两条发、之后一起回答。不并起来的话，第二条回答时第一条的调用会被判成
/// 「被打断」，它真正的结果就丢了。
fn join_split_calls(messages: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        let is_assistant = message.get("role").and_then(Value::as_str) == Some("assistant");
        let previous_calls = out
            .last()
            .map(|previous| {
                previous.get("role").and_then(Value::as_str) == Some("assistant")
                    && !previous
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .map(Vec::is_empty)
                        .unwrap_or(true)
            })
            .unwrap_or(false);
        if is_assistant && previous_calls {
            let previous = out.last().cloned().unwrap_or(Value::Null);
            let content = join_non_empty(&[
                text_of(previous.get("content")),
                text_of(message.get("content")),
            ]);
            let mut calls = previous
                .get("tool_calls")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(more) = message.get("tool_calls").and_then(Value::as_array) {
                calls.extend(more.iter().cloned());
            }
            let mut merged = previous.as_object().cloned().unwrap_or_default();
            merged.insert("content".to_string(), Value::String(content));
            merged.insert("tool_calls".to_string(), Value::Array(calls));
            if let Some(last) = out.last_mut() {
                *last = Value::Object(merged);
            }
            continue;
        }
        out.push(message.clone());
    }
    out
}

/// `argsOf`：工具的调用参数归一成一段 JSON 对象文本。
fn args_of(value: Option<&Value>) -> String {
    match value {
        Some(object @ Value::Object(_)) => object.to_string(),
        Some(Value::String(text)) => parse_object(text),
        None => "{}".to_string(),
        Some(Value::Null) => "{}".to_string(),
        Some(other) => {
            // 数字 / 布尔等非对象、非字符串：按 JSON.parse 的失败处理
            let text = other.to_string();
            parse_object(&text)
        }
    }
}

/// 解析一段 JSON 文本，非对象一律给 `{}`
fn parse_object(text: &str) -> String {
    let source = if text.is_empty() { "{}" } else { text };
    match serde_json::from_str::<Value>(source) {
        Ok(Value::Object(map)) => Value::Object(map).to_string(),
        _ => "{}".to_string(),
    }
}

/// 客户端给的工具（`type` 缺省或 `function` + 名字才算）。
struct ChatTool {
    name: String,
    description: String,
    schema: Value,
}

fn chat_tools(chat: &Value) -> Vec<ChatTool> {
    chat.get("tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|tool| {
                    let kind = tool.get("type").and_then(Value::as_str);
                    (kind.is_none() || kind == Some("function"))
                        && tool
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(Value::as_str)
                            .map(|name| !name.is_empty())
                            .unwrap_or(false)
                })
                .map(|tool| {
                    let function = tool.get("function").cloned().unwrap_or(Value::Null);
                    ChatTool {
                        name: function
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        description: function
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        schema: object_root(
                            function.get("parameters").unwrap_or(&Value::Null),
                        ),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 追加一条工具定义（`<tool_descriptions>` 里那条说明的指针）。
fn push_tool(
    out: &mut Pb,
    offered: &mut HashSet<String>,
    name: &str,
    description: &str,
    schema: &Value,
) {
    let schema = if schema.is_object() {
        schema.clone()
    } else {
        json!({ "type": "object", "properties": {} })
    };
    offered.insert(name.to_string());
    let mut tool = Pb::new();
    tool.str(1, name)
        .str(2, if description.is_empty() { name } else { description })
        .bytes(3, schema.to_string().as_bytes());
    out.bytes(10, &tool.done());
}

/// `toolDescriptions`：把工具的说明集中成一段 `<tool_descriptions>`。
fn tool_descriptions(tools: &[ChatTool]) -> String {
    let said: Vec<&ChatTool> = tools
        .iter()
        .filter(|tool| !tool.description.is_empty())
        .collect();
    if said.is_empty() {
        return String::new();
    }
    let mut out = String::from("<tool_descriptions>\n");
    for tool in said {
        out.push_str(&format!(
            "<tool name=\"{}\">\n{}\n</tool>\n",
            tool.name,
            tool.description.trim()
        ));
    }
    out.push_str("</tool_descriptions>");
    out
}

/// `metadata`：每条请求都自报的客户端身份（缺 IDE / 扩展名会让上游只列一个模型）。
pub fn metadata(token: &str) -> Vec<u8> {
    let mut pb = Pb::new();
    pb.str(1, "devin-cli")
        .str(2, CLI_VERSION)
        .str(3, token)
        .str(4, "en")
        .str(5, endpoints::os_name())
        .str(7, CLI_VERSION)
        .str(12, "chisel")
        .str(28, "chisel");
    pb.done()
}

/// `encodeMsg`：一条 `Message`
fn encode_msg(message: &OutMsg) -> Vec<u8> {
    let mut pb = Pb::new();
    pb.str(1, &random_uuid()).varint(2, message.role);
    if !message.text.is_empty() {
        pb.str(3, &message.text);
    }
    for call in &message.calls {
        let mut inner = Pb::new();
        inner
            .str(1, &call.id)
            .str(2, &call.name)
            .bytes(3, call.args.as_bytes());
        pb.bytes(6, &inner.done());
    }
    if !message.call_id.is_empty() {
        pb.str(7, &message.call_id);
    }
    for image in &message.images {
        let mut inner = Pb::new();
        inner.str(1, &image.data).str(2, &image.mime);
        pb.bytes(10, &inner.done());
    }
    pb.done()
}

/// `joinNonEmpty`：丢掉空串后 `\n\n` 连接。
fn join_non_empty(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 读一个整数字段（缺失 / 非数字给 None）
fn number_of(object: &Value, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64)
}

/// 读一个小数字段
fn number_of_float(object: &Value, key: &str) -> Option<f64> {
    object.get(key).and_then(Value::as_f64)
}

/// 恰好 0 抬到 1e-6
fn above_zero(value: f64) -> f64 {
    if value == 0.0 {
        CHAT_TOP_EPSILON
    } else {
        value
    }
}

/// uid 是不是 Gemini 系（要过 `gemini_schema`）
fn is_gemini(uid: &str) -> bool {
    uid.to_lowercase().contains("gemini")
}

/// `inlineImage`：把 data URL 拆成 (data, mime)；外链 / 无字节一律 None。
fn inline_image(part: &Value) -> Option<InlineImage> {
    if part.get("type").and_then(Value::as_str) != Some("image_url") {
        return None;
    }
    let url = match part.get("image_url") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(map)) => map
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    if data.is_empty() {
        return None;
    }
    let mime = meta.strip_suffix(";base64").unwrap_or(meta);
    let mime = if mime.is_empty() { "image/png" } else { mime };
    Some(InlineImage {
        data: data.to_string(),
        mime: mime.to_string(),
    })
}

/// `formatText`：`response_format` 用话讲出来（Devin 没有格式字段）。
pub fn format_text(format: Option<&Value>) -> String {
    let Some(format) = format else {
        return String::new();
    };
    match format.get("type").and_then(Value::as_str) {
        Some("json_object") => "Respond with a single JSON object and nothing else.".to_string(),
        Some("json_schema") => {
            let schema = format
                .get("json_schema")
                .and_then(|value| value.get("schema"))
                .or_else(|| format.get("schema"));
            match schema {
                None | Some(Value::Null) => {
                    "Respond with a single JSON object and nothing else.".to_string()
                }
                Some(schema) => format!(
                    "Respond with a single JSON value and nothing else, matching this JSON schema:\n{}.",
                    schema
                ),
            }
        }
        _ => String::new(),
    }
}

/// 安全工具调用 id：只含 `[A-Za-z0-9_-]` 且非空。
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// `outID`：Devin 给调用起的名字（`Bash:0#a65b…`）会被 Claude Code 拒收
/// （它只认 `[A-Za-z0-9_-]`），所以先改写成 `dv_` + base64url。
pub fn out_id(id: &str) -> String {
    if is_safe_id(id) && !id.starts_with("dv_") {
        return id.to_string();
    }
    format!("dv_{}", base64url(id.as_bytes()))
}

/// `inID`：把 `dv_` 改写的 id 还原回 Devin 原样（认得出来才还原）。
pub fn in_id(id: &str) -> String {
    if !id.starts_with("dv_") || !is_safe_id(id) {
        return id.to_string();
    }
    let Some(raw) = base64url_decode(&id[3..]) else {
        return id.to_string();
    };
    let Ok(raw) = String::from_utf8(raw) else {
        return id.to_string();
    };
    if !raw.is_empty() && out_id(&raw) == id {
        raw
    } else {
        id.to_string()
    }
}

use base64::Engine;

/// base64url（无填充）
fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url 解码（认不出返回 None）
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

/// 一个随机 UUID v4 字符串（每条消息的自报 id）。
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::getrandom(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ─── 工具 schema 的折叠（objectRoot / geminiSchema）──────────────────

fn is_obj(value: &Value) -> bool {
    value.is_object()
}

fn map_of(value: &Value) -> Option<Map<String, Value>> {
    value.as_object().cloned()
}

/// `objectRoot`：把工具参数 schema 折成「根就是个对象」，去掉根上的
/// anyOf / oneOf / allOf。
///
/// Claude 系的模型（Devin 的 Claude / swe-2）会拒收根上带联合类型的工具 schema。
/// 折叠规则与参考实现逐字一致（见模块头）。
pub fn object_root(schema: &Value) -> Value {
    fold_root(schema, schema, 0)
}

fn fold_root(root: &Value, schema: &Value, depth: u32) -> Value {
    if !is_obj(schema) {
        return schema.clone();
    }
    let has = |key: &str| schema.get(key).is_some();
    if schema.get("type").and_then(Value::as_str) == Some("object")
        && !has("anyOf")
        && !has("oneOf")
        && !has("allOf")
    {
        return schema.clone();
    }
    let mut out = map_of(schema).unwrap_or_default();
    let mut props = schema
        .get("properties")
        .and_then(map_of)
        .unwrap_or_default();
    let mut required: Vec<Value> = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // 本地的 $ref 解析（只认 $defs / definitions 前缀）
    let resolve = |branch: &Value| -> Option<Value> {
        if !is_obj(branch) {
            return None;
        }
        let reference = branch.get("$ref").and_then(Value::as_str);
        let Some(reference) = reference else {
            return Some(branch.clone());
        };
        if reference.is_empty() {
            return Some(branch.clone());
        }
        for key in ["$defs", "definitions"] {
            let prefix = format!("#/{key}/");
            if let Some(name) = reference.strip_prefix(&prefix) {
                let definition = root.get(key).and_then(|value| value.get(name));
                return definition.filter(|value| is_obj(value)).cloned();
            }
        }
        None
    };

    // allOf：各支的属性与 required 全并进来
    if let Some(branches) = schema.get("allOf").and_then(Value::as_array) {
        for branch in branches {
            let Some(merged) = resolve(branch) else {
                continue;
            };
            if let Some(merged_props) = merged.get("properties").and_then(map_of) {
                for (key, value) in merged_props {
                    props.entry(key).or_insert(value);
                }
            }
            if let Some(more) = merged.get("required").and_then(Value::as_array) {
                required.extend(more.iter().cloned());
            }
        }
    }
    out.remove("allOf");

    let mut branches: Vec<Value> = Vec::new();
    for key in ["anyOf", "oneOf"] {
        if let Some(items) = schema.get(key).and_then(Value::as_array) {
            for branch in items {
                let Some(mut merged) = resolve(branch) else {
                    continue;
                };
                if depth < 8
                    && ["anyOf", "oneOf", "allOf"].iter().any(|union| merged.get(*union).is_some())
                {
                    merged = fold_root(root, &merged, depth + 1);
                }
                let is_object = merged.get("type").and_then(Value::as_str) == Some("object")
                    || merged.get("properties").is_some();
                if !is_object {
                    continue;
                }
                branches.push(merged);
            }
        }
        out.remove(key);
    }

    // 每个分支都要求的字段保持 required；根没定义的字段取各分支给的 schema
    let mut common: Option<Vec<Value>> = None;
    let mut from: std::collections::HashMap<String, Vec<Value>> = std::collections::HashMap::new();
    for branch in &branches {
        if let Some(branch_props) = branch.get("properties").and_then(map_of) {
            for (key, value) in branch_props {
                if props.contains_key(&key) {
                    continue;
                }
                let list = from.entry(key).or_default();
                let text = value.to_string();
                if !list.iter().any(|item| item.to_string() == text) {
                    list.push(value);
                }
            }
        }
        let branch_required = branch
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        common = Some(match common {
            None => branch_required,
            Some(previous) => previous
                .into_iter()
                .filter(|item| branch_required.contains(item))
                .collect(),
        });
    }
    for (key, values) in from {
        if values.len() == 1 {
            props.insert(key, values.into_iter().next().unwrap_or(Value::Null));
        } else {
            props.insert(key, json!({ "anyOf": values }));
        }
    }
    for item in common.unwrap_or_default() {
        if !required.contains(&item) {
            required.push(item);
        }
    }
    // required 去重（保序）
    let mut seen: Vec<String> = Vec::new();
    required.retain(|item| {
        let key = item.to_string();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });

    out.insert("type".to_string(), Value::String("object".to_string()));
    out.insert("properties".to_string(), Value::Object(props));
    if required.is_empty() {
        out.remove("required");
    } else {
        out.insert("required".to_string(), Value::Array(required));
    }
    Value::Object(out)
}

/// `geminiSchema`：把工具 schema 折成 Devin 的 Gemini 系模型能收的形态。
///
/// Gemini 拒收「类型是数组」（`["string","null"]`）、「无 items 的数组」、
/// 「items 是数组或 true」、「有 properties / enum 但没 type」这些形态。
/// 逐字移植参考实现的 `geminiSchema`。
pub fn gemini_schema(schema: &Value, depth: u32) -> Value {
    if !is_obj(schema) || depth > 32 {
        return schema.clone();
    }
    let mut out = map_of(schema).unwrap_or_default();

    let sub = |value: &Value| gemini_schema(value, depth + 1);

    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(object) = out.get(key).and_then(map_of) {
            let mapped = object
                .into_iter()
                .map(|(name, value)| (name, sub(&value)))
                .collect::<Map<_, _>>();
            out.insert(key.to_string(), Value::Object(mapped));
        }
    }
    for key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(items) = out.get(key).and_then(Value::as_array).cloned() {
            out.insert(
                key.to_string(),
                Value::Array(items.iter().map(sub).collect()),
            );
        }
    }
    for key in [
        "additionalProperties",
        "not",
        "if",
        "then",
        "else",
        "contains",
    ] {
        if let Some(value) = out.get(key).cloned() {
            if is_obj(&value) {
                out.insert(key.to_string(), sub(&value));
            }
        }
    }

    // 类型是数组：折成一个类型（含 null 时置 nullable），或 anyOf 展开
    if let Some(Value::Array(types)) = out.get("type").cloned() {
        let mut unique: Vec<String> = Vec::new();
        for value in types {
            if let Some(text) = value.as_str() {
                if !unique.iter().any(|item| item == text) {
                    unique.push(text.to_string());
                }
            }
        }
        let types = unique;
        let some: Vec<String> = types
            .iter()
            .filter(|value| value.as_str() != "null")
            .cloned()
            .collect();
        if some.len() < types.len() && !some.is_empty() {
            out.insert("nullable".to_string(), Value::Bool(true));
        }
        if some.len() == 1 {
            out.insert("type".to_string(), Value::String(some[0].clone()));
        } else if some.is_empty() {
            if types.is_empty() {
                out.remove("type");
            } else {
                out.insert("type".to_string(), Value::String("null".to_string()));
            }
        } else {
            out.remove("type");
            let branches: Vec<Value> = some
                .iter()
                .map(|kind| {
                    let mut branch = Map::new();
                    branch.insert("type".to_string(), Value::String(kind.clone()));
                    if kind == "array" {
                        branch.insert("items".to_string(), Value::Object(Map::new()));
                    }
                    Value::Object(branch)
                })
                .collect();
            let existing = out
                .remove("anyOf")
                .and_then(|value| value.as_array().cloned())
                .unwrap_or_default();
            let mut merged = existing;
            merged.extend(branches);
            out.insert("anyOf".to_string(), Value::Array(merged));
        }
    }

    // 没有 type 但有 enum：从取值推导类型（混合取值就丢掉 enum）
    if !out.contains_key("type") {
        if let Some(Value::Array(values)) = out.get("enum").cloned() {
            let kept: Vec<Value> = values
                .iter()
                .filter(|value| !value.is_null())
                .cloned()
                .collect();
            if kept.len() < values.len() {
                out.insert("nullable".to_string(), Value::Bool(true));
            }
            let mut kinds: Vec<&str> = Vec::new();
            for value in &kept {
                let kind = if value.is_i64() || value.is_u64() {
                    "integer"
                } else if value.is_f64() {
                    "number"
                } else if value.is_string() {
                    "string"
                } else if value.is_boolean() {
                    "boolean"
                } else {
                    "other"
                };
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
            let one = if kinds.len() == 1 {
                kinds[0].to_string()
            } else if kinds.len() == 2 && kinds.contains(&"integer") && kinds.contains(&"number") {
                "number".to_string()
            } else {
                String::new()
            };
            if ["string", "integer", "number", "boolean"].contains(&one.as_str()) {
                out.insert("type".to_string(), Value::String(one));
                out.insert("enum".to_string(), Value::Array(kept));
            } else {
                out.remove("enum");
            }
        }
    }

    // 有 properties 但没 type → object
    if !out.contains_key("type") && out.get("properties").map(is_obj).unwrap_or(false) {
        out.insert("type".to_string(), Value::String("object".to_string()));
    }
    // 数组：items 折成单 schema 或空对象
    let is_array = out.get("type").and_then(Value::as_str) == Some("array")
        || (!out.contains_key("type") && out.contains_key("items"));
    if is_array {
        let items = out.get("items").cloned().unwrap_or(Value::Null);
        let folded = if let Value::Array(list) = &items {
            if list.len() == 1 && is_obj(&list[0]) {
                sub(&list[0])
            } else {
                Value::Object(Map::new())
            }
        } else if is_obj(&items) {
            sub(&items)
        } else {
            Value::Object(Map::new())
        };
        out.insert("items".to_string(), folded);
        out.insert("type".to_string(), Value::String("array".to_string()));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    //! 用手搭的 chat 请求体喂进 `build_get_chat_message`，再把出来的 protobuf 解回去
    //! 逐条对。全部离线，钉的是参考实现那几条 shaping 规则。
    use super::*;
    use crate::server::core::providers::devin::pb::fields;

    /// 解出请求体里的消息列表（每条 `Message` 的角色与字段）
    fn messages(bytes: &[u8]) -> Vec<(u64, String, Vec<u8>)> {
        fields(bytes)
            .into_iter()
            .filter(|field| field.num == 3 && field.wire == 2)
            .map(|field| {
                let mut role = 0u64;
                let mut text = String::new();
                let mut call_id = Vec::new();
                for inner in fields(&field.data) {
                    match (inner.num, inner.wire) {
                        (2, 0) => role = inner.n,
                        (3, 2) => text = String::from_utf8_lossy(&inner.data).to_string(),
                        (7, 2) => call_id = inner.data.clone(),
                        _ => {}
                    }
                }
                (role, text, call_id)
            })
            .collect()
    }

    fn uid_of(bytes: &[u8]) -> String {
        fields(bytes)
            .into_iter()
            .find(|field| field.num == 21 && field.wire == 2)
            .map(|field| String::from_utf8_lossy(&field.data).to_string())
            .unwrap_or_default()
    }

    #[test]
    fn the_system_prompt_is_folded_into_the_first_user_message() {
        let chat = json!({
            "model": "swe-2",
            "messages": [
                { "role": "system", "content": "You are terse." },
                { "role": "user", "content": "hi" },
            ],
        });
        let bytes = build_get_chat_message(&chat, "swe-2-high", "tok");
        assert_eq!("swe-2-high", uid_of(&bytes));
        let msgs = messages(&bytes);
        assert_eq!(1, msgs.len(), "system 消息不该单独发出去");
        assert_eq!(ROLE_USER, msgs[0].0);
        assert!(msgs[0].1.starts_with("You are terse."), "系统段要拼在第一条 user 开头：{}", msgs[0].1);
        assert!(msgs[0].1.contains("hi"));
    }

    #[test]
    fn an_unanswered_tool_call_is_answered_with_the_interrupted_result() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": "run it" },
                { "role": "assistant", "content": "", "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "Bash", "arguments": "{\"cmd\":\"ls\"}" } }
                ]},
            ],
        });
        let bytes = build_get_chat_message(&chat, "claude-opus-5", "tok");
        let msgs = messages(&bytes);
        // user + assistant + 被打断的 tool
        let tool = msgs
            .iter()
            .find(|(role, ..)| *role == ROLE_TOOL)
            .expect("应当补一条 tool 消息");
        assert_eq!(NO_RESULT, tool.1);
        assert_eq!(b"call_1".to_vec(), tool.2, "call_id 要原样对上");
    }

    #[test]
    fn a_non_data_image_is_rejected() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": [
                    { "type": "image_url", "image_url": { "url": "https://example.invalid/a.png" } }
                ]},
            ],
        });
        let bytes = build_get_chat_message(&chat, "swe-2", "tok");
        let msgs = messages(&bytes);
        // 只有外链图片、没有文字 ⇒ 整条 user 消息被丢掉，最后补一条「继续」
        assert_eq!(1, msgs.len(), "只剩兜底的那条 user");
        assert_eq!(ROLE_USER, msgs[0].0);
        // image 字段（10）一个都不该有
        let images = fields(&bytes)
            .into_iter()
            .filter(|field| field.num == 3 && field.wire == 2)
            .flat_map(|field| fields(&field.data))
            .filter(|inner| inner.num == 10)
            .count();
        assert_eq!(0, images, "外链图片不该进图片字段");
    }

    #[test]
    fn inline_data_images_are_kept() {
        let chat = json!({
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "look" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
                ]},
            ],
        });
        let bytes = build_get_chat_message(&chat, "swe-2", "tok");
        let images: Vec<Vec<u8>> = fields(&bytes)
            .into_iter()
            .filter(|field| field.num == 3 && field.wire == 2)
            .flat_map(|field| fields(&field.data))
            .filter(|inner| inner.num == 10 && inner.wire == 2)
            .map(|inner| {
                fields(&inner.data)
                    .into_iter()
                    .find(|f| f.num == 1)
                    .map(|f| f.data)
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(vec![b"QUJD".to_vec()], images, "内联字节要进图片字段");
    }

    #[test]
    fn out_id_and_in_id_round_trip_only_for_rewritten_ids() {
        // 安全 id 原样出去、原样回来
        assert_eq!("call_1", out_id("call_1"));
        assert_eq!("call_1", in_id("call_1"));
        // Devin 的 `Bash:0#…` 不安全，要被改写，且能还原
        let raw = "Bash:0#a65b6a5e";
        let rewritten = out_id(raw);
        assert!(rewritten.starts_with("dv_"));
        assert_eq!(raw, in_id(&rewritten));
        // 认不出的 dv_ 前缀原样返回
        assert_eq!("dv_not_base64!", in_id("dv_not_base64!"));
    }

    #[test]
    fn temperature_and_top_p_of_zero_are_lifted_above_zero() {
        let chat = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "temperature": 0,
            "top_p": 0,
        });
        let bytes = build_get_chat_message(&chat, "swe-2", "tok");
        let config = fields(&bytes)
            .into_iter()
            .find(|field| field.num == 8 && field.wire == 2)
            .expect("应当有模型配置");
        let mut temperature = 0f64;
        let mut top_p = 0f64;
        for field in fields(&config.data) {
            let value = f64::from_le_bytes(field.data.as_slice().try_into().unwrap_or([0; 8]));
            if field.num == 5 {
                temperature = value;
            } else if field.num == 8 {
                top_p = value;
            }
        }
        assert!((temperature - CHAT_TOP_EPSILON).abs() < 1e-12, "temperature 0 要抬到 1e-6");
        assert!((top_p - CHAT_TOP_EPSILON).abs() < 1e-12, "top_p 0 要抬到 1e-6");
    }

    #[test]
    fn object_root_folds_a_root_level_one_of_into_an_object() {
        let schema = json!({
            "oneOf": [
                { "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] },
                { "type": "object", "properties": { "b": { "type": "number" } }, "required": ["b"] },
            ]
        });
        let folded = object_root(&schema);
        assert_eq!("object", folded["type"].as_str().unwrap());
        assert_eq!("string", folded["properties"]["a"]["type"].as_str().unwrap());
        assert_eq!("number", folded["properties"]["b"]["type"].as_str().unwrap());
        assert!(folded.get("oneOf").is_none(), "根上的 oneOf 要去掉");
        // 两个分支各自要求的字段都不算「共同要求」
        assert!(folded.get("required").is_none());
    }

    #[test]
    fn object_root_merges_all_of_and_keeps_common_requirements() {
        let schema = json!({
            "allOf": [
                { "type": "object", "properties": { "a": { "type": "string" } }, "required": ["a"] }
            ]
        });
        let folded = object_root(&schema);
        assert_eq!("string", folded["properties"]["a"]["type"].as_str().unwrap());
        assert_eq!(json!(["a"]), folded["required"]);
    }

    #[test]
    fn gemini_schema_collapses_a_nullable_type_list() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": ["string", "null"] },
            },
        });
        let folded = gemini_schema(&schema, 0);
        assert_eq!("string", folded["properties"]["name"]["type"].as_str().unwrap());
        assert_eq!(true, folded["properties"]["name"]["nullable"].as_bool().unwrap());
    }

    #[test]
    fn tool_descriptions_block_is_only_emitted_for_described_tools() {
        let tools = vec![
            ChatTool {
                name: "Bash".to_string(),
                description: "Run a shell command".to_string(),
                schema: Value::Null,
            },
            ChatTool {
                name: "Anon".to_string(),
                description: String::new(),
                schema: Value::Null,
            },
        ];
        let block = tool_descriptions(&tools);
        assert!(block.starts_with("<tool_descriptions>"));
        assert!(block.contains("<tool name=\"Bash\">"));
        assert!(!block.contains("Anon"), "没描述的工具不进说明块");
    }

    #[test]
    fn format_text_words_the_response_format() {
        assert_eq!(
            "Respond with a single JSON object and nothing else.",
            format_text(Some(&json!({ "type": "json_object" })))
        );
        assert!(format_text(None).is_empty());
        assert!(format_text(Some(&json!({ "type": "text" }))).is_empty());
    }

    #[test]
    fn noted_reads_the_marker_and_strips_the_blank_line_before_it() {
        //! 回归：参考实现的正则是 `... they) follows? in ...`，分组与 `follows?` 之间
        //! **有空格**，所以标记与后缀之间的 body 带尾随空格。曾经不 trim → 两条判据
        //! 恒失败 → `noted` 永远返回 0 → 注记摘不掉、工具图片回挂整条链变死代码。
        let (count, body) = noted(
            "cards\n\n[The tool returned 2 images; they follow in the next message.]",
        );
        assert_eq!(2, count);
        assert_eq!("cards", body, "标记前的空行也要一起摘掉");

        // 单数形态
        let (count, body) = noted("[The tool returned an image; it follows in the next message.]");
        assert_eq!(1, count);
        assert_eq!("", body);

        // 与这套标记无关的正文原样返回
        let (count, body) = noted("nothing to see here");
        assert_eq!(0, count);
        assert_eq!("nothing to see here", body);
    }

    #[test]
    fn noted_to_result_for_to_settle_strips_the_note_and_keeps_images() {
        //! 三件套的联动：`noted` 读出「有几张图跟随」→ `result_for` 认出这是哪个刚发出
        //! 的工具结果 → `settle_results` 在图片齐了之后把注记从正文里摘掉。`noted` 曾经
        //! 恒返回 0（body 带尾随空格），这条链会整条静默失效：注记留在结果正文里，
        //! 工具回传的图片也回挂不上。
        const MARKED: &str =
            "img-1\n\n[The tool returned 2 images; they follow in the next message.]";
        let (said_n, said_text) = noted(MARKED);
        assert_eq!(2, said_n);
        assert_eq!("img-1", said_text);

        let mut msgs = vec![OutMsg {
            role: ROLE_TOOL,
            text: MARKED.to_string(),
            calls: Vec::new(),
            call_id: "call_1".to_string(),
            images: Vec::new(),
        }];
        let header = format!("{FROM}tool call call_1:]");

        // ① 图片还没回来（`seen < said_n`）：`result_for` 认得出这条抬头，注记先留着
        let mut results = vec![ResultEntry {
            msg_index: 0,
            id: "call_1".to_string(),
            said_n,
            said_text: said_text.clone(),
            seen: 0,
        }];
        assert_eq!(Some(0), result_for(&header, &results));
        settle_results(&mut msgs, &mut results);
        assert!(
            msgs[0].text.contains("[The tool returned"),
            "图片没齐就不该摘注记"
        );

        // ② 两张都回来了（`seen == said_n`）：注记摘掉，正文只剩结果本身
        let mut results = vec![ResultEntry {
            msg_index: 0,
            id: "call_1".to_string(),
            said_n,
            said_text,
            seen: 2,
        }];
        // 已经结算过的条目不再命中 `result_for`（它只服务「还差图片」的那条）
        assert_eq!(None, result_for(&header, &results));
        settle_results(&mut msgs, &mut results);
        assert_eq!("img-1", msgs[0].text, "图片齐了就该把注记摘掉");
    }
}
