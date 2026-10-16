# Agent2API 评审问题修复方案（最小化改动版）

> 依据：三路资深评审（brooks-review / superpowers requesting-code-review / dev-code-review）的结论，
> 每一条都**由我在本仓源码上逐点复核**（复核状态标在各项的「复核」行）。
> 参考实现：`yetone/magpie`（Go，内置 Devin/SystemOne）+ `magpie-community/plugins`（JS，移植源）。
> 专业资料锚点：*Code Complete* Ch.8（防御式编程：边界校验必须对**未信任输入**成立）、
> *A Philosophy of Software Design*（深模块 / 信息隐藏 / 消灭死代码）、
> *xUnit Test Patterns*（行为验证——测试必须能因被测行为错误而失败）。

---

## 0. 总览

| # | 级别 | 落点 | 一句话 |
|---|---|---|---|
| F1 | 🔴 Blocker | `devin/pb.rs` `fields()` | protobuf 长度前缀 usize 加法回绕 → `bytes[i..i+len]` panic → 进程 abort |
| F2 | 🟡 Should | `devin/chat.rs` `noted()` | 尾随空格使 `strip_suffix` / 相等判据恒失败 → 工具图片回挂整条链变死代码 |
| F3 | 🟡 Should | `api/accounts.rs` `refresh_account` | 漏 `devin` 分派臂 → 掉进 workbuddy 兜底，报风马牛不相及的错误 |
| F4 | 🟡 Should | `devin/mod.rs` `record_proxy` | 代理写坏时**静默直连**，与自述注释和 codearts 纪律矛盾 |
| F5 | 🟡 Should | `devin/pb.rs` `gunzip` | 解压无输出上限（只限压缩侧）→ 压缩炸弹 OOM |
| F6 | 🟡 Should | `devin/mod.rs` 三条收帧路径 | 半帧 EOF 未校验，`FrameDecoder::finish()` 仅测试在用 → 截断被当正常结束 |
| F7 | 🟡 Should | `custom_providers.rs` `is_opencode_zen_free` | 目录取不到时缺内置免费白名单；且未做「按协议判可否聊天」 |
| F8 | 🔵 Nit | `upstream/payload.rs` | 每次上游发送（含全部内置家）都为判 Zen 克隆整份配置 |
| F9 | 🔵 Nit | `login/devin.rs` | 带身份校验的导入函数是死代码，实际走的是**不校验**的入口 |
| F10 | 🔵 Nit | `devin/models.rs` | `free`/`prices`/`cost` 整条计算链**无消费者** |
| F11 | 🔵 Nit | `devin/chat.rs:515` | release 路径 `unwrap()`（违仓库硬规则） |
| F12 | 🔵 Nit | `devin/endpoints.rs` `os_name` | macOS 报 `macos`，参考实现报 `darwin`；注释还自称「逐字一致」 |
| F13 | 🔵 Nit | `devin/stream.rs` ↔ `mod.rs` | `ChunkWriter::delta` 与 `writer_delta` 重复拼装逻辑 |
| F14 | 🔵 Nit | `devin/endpoints.rs` | `DEFAULT_WEBAPP_HOST` / `DEFAULT_API_URL` 无人引用 |
| F15 | 🔵 Nit | `custom_providers.rs` `fetch_upstream_models` | SystemOne 家点「获取模型」会去打不存在的 `…/systemone/v1/models` |
| F16 | 🔵 Nit | `devin/oauth.rs` 测试 | 测试名说测身份映射，实际只断言 `classify_failure` → 同义反复 |
| F17 | 🔵 Nit | 测试面 | `noted`/`result_for`/`fields` 溢出/解压上限/真实 88 条 id 均无测试 |
| F18 | 🔵 Nit | `custom_providers.rs` | `model_is_retired` 取值口径与参考的 `status ?? lifecycle` 不同；写侧大小写敏感、读侧不敏感 |
| F19 | 🔵 Nit | `api/custom_providers.rs` `describe_changes` | 字段清单漏 `modelFilter`（只影响「改了什么」的提示） |

**改动面预估**：F1–F7 共约 **9 处函数体 + 1 处分派臂**；F8–F19 为**单行/单函数**级；**零新增依赖、零新增模块、零接口破坏**（仅 F4 给一个私有函数换返回类型，3 个调用点补 `?`）。

---

## 1. 修复设计（逐项）

### F1 🔴 `devin/pb.rs` — protobuf 长度前缀溢出（唯一崩溃项）

**复核：已确认可达。** `read_uvarint` 最多吃 10 字节、`shift >= 64` 才截断 → 可产出 `2^64-8`；
[`fields()` L157-161](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/devin/pb.rs#L157-L161) 的 `if index + length > bytes.len()` 是 **usize 加法**，release（`overflow-checks` 默认关）回绕 → 守卫失效 → `bytes[index..index + length]` 因 start>end panic。
输入 `[0x12, <10 字节 varint = 2^64-8>, 'a', 'b']`：`index=11`，`11+len` 回绕成 `3`，`3 > 13` 不成立 → `bytes[11..3]` panic。
`[profile.release] panic="abort"` → **一次畸形帧 = 网关进程 abort**；入口是上游可控字节（目录接口 / 回复流 / 三个模型解析器），且账号上的 `api_server_url` 允许把 base 指向任意主机。

**最小改法**（只动 wire-2 分支，其它分支的 `+8` / `+4` 是常量、不可能回绕）：

```rust
2 => {
    let Some(length) = read_uvarint(bytes, &mut index) else { break };
    // 先按 u64 比一次（长度本身可能大得离谱），再 checked_add 防 usize 回绕 ——
    // 参考实现是 JS 浮点比较（不回绕），这里必须显式挡住（Code Complete Ch.8）
    if length > bytes.len() as u64 {
        break;
    }
    let length = length as usize;
    let Some(end) = index.checked_add(length) else { break };
    if end > bytes.len() {
        break;
    }
    field.data = bytes[index..end].to_vec();
    index = end;
}
```

**复用/最小性**：不引入任何依赖，不换容器，语义仍是「畸形尾部丢弃」（与参考 `fields()` 的「丢尾巴不废整包」一致）。
**验证**：新增单测 `a_huge_length_prefix_does_not_panic`（喂上面那串字节，断言**不 panic** 且返回空/截断）；另加 `length_prefix_past_the_end_is_dropped`。

---

### F2 🟡 `devin/chat.rs` `noted()` — 尾随空格（功能链整条失效）

**复核：已确认。** 参考实现（`index.mjs:1353`）的正则是 `... they) follows? in ...`，分组与 `follows?` 之间**有一个空格**；`[The tool returned 3 images; they follow in the next message.]` 里 `MARKER` 与后缀之间的 `body` 是 `"3 images; they "`（**尾随空格**）。
而 [chat.rs:430](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/devin/chat.rs#L430) 的 `strip_suffix(" images; they")` 与 [:438](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/devin/chat.rs#L438) 的 `body != "an image; it"` 都不吃那个空格 → 恒返回 `(0, text)`。
后果：`said_n` 恒 0 ⇒ `result_for` 永不命中、`settle_results` 永不摘注记/回挂图片 —— 模块头明说要做的那套机制**整体变死代码**。

**最小改法**（一处，两侧都覆盖）：

```rust
let body = body.trim_end();   // 参考实现在标记与 follows 之间留了一个空格
```

**复用/最小性**：不动函数签名、不动调用点、不改参考语义；一个 `trim_end()` 同时修好复数/单数两条路径，且对未来上游微调（多一个空格）也稳。
**验证**：新增 `noted` 的三态单测 —— 复数（`3`）、单数（`1`）、无注记（`0` 且原样返回）；再补 `result_for` 的命中/未命中各一条（评审指出的最大测试缺口就在这里）。

---

### F3 🟡 `api/accounts.rs` `refresh_account` — 补 devin 分派臂

**复核：已确认。** 全文件 `devin` 只出现在 `add_account`（:886-899）；[`refresh_account`](file:///workspace/desktop-tauri/src-tauri/server/src/server/api/accounts.rs#L1240-L1293) 的逐家 `if … is_some()` 链在 CatPaw / MonkeyCode / Command Code 之后**没有 devin**，直接落到末尾的 workbuddy 兜底（`state.auth().refresh_account(&id)`）→ 用户收到一条腾讯鉴权端点方向的错。
本仓注释自己把这条列为「加带 refreshToken 的家时必须一起加」，Devin 同为 `supports_refresh=false`。

**最小改法**：在 workbuddy 兜底之前**照抄既有三段的形状**插一段（零新机制）：

```rust
// Devin：同样**没有刷新机制**（会话令牌不按周期过期、上游无 refresh 接口，
// 见 `providers::devin` 与 `core::login::devin` 的模块头）。不显式拦下会落到
// 下面的 workbuddy 兜底链路，用户收到一条与腾讯鉴权端点相关的错 —— 与
// CatPaw / MonkeyCode / Command Code 漏登记时同一性质。
if state.store().devin_account_record(&id).is_some() {
    return management_error(
        400,
        "Devin 的登录态没有刷新机制（会话令牌不按周期过期、上游无续期接口）：\
         请在浏览器重新走一次网页登录，或重新导入本机 devin CLI 的 credentials.toml",
    );
}
```

**复用/最小性**：与既有三段逐字同构（同一个 `management_error(400, …)`、同一种拦截理由），不新增分支语义。

---

### F4 🟡 `devin/mod.rs` `record_proxy` — 对齐 codearts（不静默直连）

**复核：已确认。** [devin/mod.rs:631-642](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/devin/mod.rs#L631-L642) 的注释写着「解析失败不静默降级成直连」，代码却是 `_ => None`；codearts 的[同名函数](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/codearts/mod.rs#L772-L785)对 `Failed` 返回 400。

**最小改法**：**照抄 codearts 的函数体**（返回 `Result`，`Failed → 400`，缺账号 → 503），并把 devin 现有的两处调用点补 `?`：

| 调用点 | 现状 | 改后 |
|---|---|---|
| `account_credential`（:628） | `Ok((credential, record_proxy(...)))` | `Ok((credential, record_proxy(store, account_id)?))` |
| 目录刷新（:181） | `let proxy = record_proxy(store, account_id);` | `let proxy = record_proxy(store, account_id)?;` |
| `forward_conversation`（:274） | 经 `account_credential` | 无需改（`?` 已在其上游） |

**复用/最小性**：直接复用 codearts 已定的口径与 `ProxyResolution` 枚举，**不新建类型**。

---

### F5 🟡 `devin/pb.rs` `gunzip` — 加解压上限，并做成可测

**复核：已确认。** [gunzip](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/devin/pb.rs#L286-L293) 用 `read_to_end`，只受**压缩侧** `MAX_FRAME_BYTES`（64 MiB）约束 → 可膨胀到数 GiB（OOM）。参考实现同样没挡（`gunzipSync`），但本仓已专门设了压缩上限，属「只挡了一半」。

**最小改法**（含可测性）：

```rust
fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    gunzip_capped(data, MAX_FRAME_BYTES + 1)
}

/// `cap` 是**解压后**的字节上限：超出即判压缩炸弹（README 的 64 MiB 压缩上限
/// 挡的是压缩前，两者不是一回事）。
fn gunzip_capped(data: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .take(cap as u64)
        .read_to_end(&mut out)
        .map_err(|error| format!("Connect 帧 gzip 解压失败: {error}"))?;
    if out.len() >= cap {
        return Err("Connect 帧解压后超限（疑似压缩炸弹）".to_string());
    }
    Ok(out)
}
```

**复用/最小性**：复用已有的 `MAX_FRAME_BYTES` 与 `flate2`；拆出 `gunzip_capped` 只为让「超限」这条分支**可测**（否则 64 MiB 的用例跑不动）。
**验证**：`gunzip_capped(small_gz, 8)` 断言报错；既有 `pb.rs:396` 的正常 gzip 用例继续过。

---

### F6 🟡 半帧 EOF 未校验 — 在收帧路径调用已有的 `finish()`

**复核：已确认。** [`FrameDecoder::finish()`](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/devin/pb.rs#L243-L250) 语义正确（残留即「意外 EOF」），但全仓只有 `pb.rs:372`（测试）调用它；`mod.rs` 三条收尾（首帧预读 / `drive_stream` / 聚合）在源流返回 `None` 时都不校验 → 半帧被丢弃、请求被当成**干净完成**（参考实现的 `frames()` 在 `reader done` 且 `buf.length` 时抛 `"unexpected EOF"`）。

**最小改法**：在 `mod.rs` 的三条收尾点各加一行（复用已有函数，不新增逻辑）：

```rust
decoder.finish().map_err(|reason| GatewayError::with_status(502, reason))?;
```

**复用/最小性**：函数已存在（当初就写好了，只是没接线）；三处一行。
**验证**：单测——喂半个帧后结束源流，断言得 502 而非「正常完成」。

---

### F7 🟡 `custom_providers.rs` — 退化白名单 + 协议判据（对齐方案 §B2）

**复核：已确认两点。**
1. [`is_opencode_zen_free`](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/custom_providers.rs#L1148-L1171) 在 `catalog = None` 时只剩「id 含 `free`」→ **`big-pickle`（零代价、无后缀）被丢**；而我当初还把这个退化**写进单测**当预期（`zen_free_without_catalog_falls_back_to_id_rule`）。参考实现的 `discover()` 在目录失败时用 `bootstrapProvider` 兜底（含 `big-pickle` cost 0/0）。
2. 只用硬编码单条排除表（`OPENCODE_ZEN_NON_CHAT_IDS = ["jev-1.13-free"]`），没做「按目录/文档判协议」。**今天**两支输出一致（实测 systemone 免费模型只有 `jev-1.13-free`，恰好被覆盖），但新增的 systemone 免费模型会被错误广告成可聊模型。

**最小改法**（两行 + 一张小表，全部复用已有结构）：

```rust
/// 目录取不到时的**内置免费白名单**：与参考实现 `bootstrapProvider` 同一角色
/// —— 没有 `-free` 后缀、但目录里是 0 代价的免费模型（当前只有 big-pickle）。
const OPENCODE_ZEN_FREE_ALLOWLIST: &[&str] = &["big-pickle"];
```

- `is_opencode_zen_free`：`if id 含 free || 白名单命中 → true;`（目录有无都先吃这一层）。
- 把 `OPENCODE_ZEN_NON_CHAT_IDS` 的注释改成**「官方 models.mdx 协议表的快照」**，并写明刷新方式（表里 systemone 仅 `jev-1.13-free`）；同时把 `listedFree`-式判据留作 future 扩展点。
- 单测改为断言退化时 `big-pickle` **保留**（把原错误预期改掉），并补一条「无后缀且非白名单 → 丢」。

**决定（需你确认，见 §4）**：是否顺带把「按目录 `provider.npm` 判协议」这一层补上（多约 10 行，做到与参考实现同构）；不补则维持「快照表 + 注释」的最小版本。

---

### F8 🔵 `upstream/payload.rs` — 热路径短路（一行）

**复核：已确认。** [payload.rs:183](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/upstream/payload.rs#L183) 对**每一次**上游发送都调 `protocol_of(provider_id)` → `get` → `read_items` → `config::current()`（本仓注释明写 `current()` **每次克隆整份 raw**，并专门为热路径留了轻量访问器）。内置家占绝大多数流量，却每次都要走一遍才拿到 `None`。

**最小改法**：复用已有的 `ID_PREFIX` 先短路：

```rust
if provider_id.starts_with(crate::server::core::custom_providers::ID_PREFIX)
    && crate::server::core::custom_providers::protocol_of(provider_id).as_deref()
        == Some(crate::server::core::custom_providers::PROTOCOL_SYSTEMONE)
{
```

---

### F9 🔵 `login/devin.rs` — 让「验过身份」的那条路成为唯一路

**复核：已确认。** `import_devin_cli_login`（:36，含 `who_by_key` 校验、补邮箱、失效令牌报 401）与 `start_devin_login`（:27）**全仓无调用**；实际入口是 [api/accounts.rs:888](file:///workspace/desktop-tauri/src-tauri/server/src/server/api/accounts.rs#L888) 的 `login::devin::cli_credential()`（:60）—— 它只 `read_cli_credentials()`，**不校验**。于是导入一枚已失效的令牌会静默落成一条「用一次就 401」的账号（正是被弃函数文档里想避免的）。参考实现 `cliSignIn` 是**会**调 `success(key, server)` 验身份的。

**最小改法（推荐 a）**：`api/accounts.rs` 的 `import_desktop` 分支改调 `import_devin_cli_login`（该函数已完整写好、返回 public 形态；与手动粘贴共用 `add_devin_account` 落盘），并删掉 `start_devin_login` 那段薄封装（网页登录走通用 `start_web_login(kind)`）。
**备选 b**（更小但不如 a 忠实）：把 `cli_credential()` 的注释改成「**不校验**」的事实，删掉两个未接线函数。

**复用/最小性**：a 是「接线到已写好的函数」——净删除一个死函数、换一个调用点；b 是纯删除 + 改注释。

---

### F10–F19（单点小改，逐条一行/一处）

| # | 改法（最小） | 复用 |
|---|---|---|
| F10 | `ListedModel.free/cost` 加一行注释「**当前无消费者**，为将来的免费标记 / 余额保留（方案 §D4 明确不做余额）」；或删掉 `listed()` 的 `free/prices` 入参与 `parse_free`/`parse_prices`（更大改动，可选） | 保留参考实现的解析（与 magpie 同构） |
| F11 | `chat.rs:515` → `Some(object @ Value::Object(_)) => object.to_string(),` | 无 |
| F12 | `os_name()` 的 macOS 分支改 `"darwin"`，并把注释从「逐字一致」改成「与参考实现一致（Node 的 process.platform）」 | 与参考 `osName()` 对齐 |
| F13 | 保留其一：让 `mod.rs` 的 `writer_delta` 改调 `ChunkWriter::delta`（若无需 `&mut`），否则删 `ChunkWriter::delta` 并把其测试改指生产路径 | 同一份拼装逻辑只留一处 |
| F14 | 若 `credentials.rs` 里存在同值字面量，改为引用这两个常量；否则删除 | 复用已定义的常量 |
| F15 | `fetch_upstream_models` 开头：`if protocol == PROTOCOL_SYSTEMONE { return Err("该上游（SystemOne）没有 /models 端点，请到「模型」页手动登记模型".into()) }` | 复用函数内已有的 `protocol` 局部变量 |
| F16 | 把该测试改成真的驱动 `who_by_key` 的判定（把 401→Refused 的映射抽成纯函数再单测），或至少改名成它实际测的东西 | 无 |
| F17 | 补测试：F1 / F2 / F5 / F6 各一条；`apply_model_filter` 用**冻结的真实 88 条 id 响应 + 目录**做 fixture（含 `beam-free`、`deepseek-v4.1-flash-free`、`status=deprecated` 的 `muse-spark-1.2-contributor-free`）+ 一条「`modelFilter` 为空 → 原样透传」 | 复用已有的 json fixture 风格 |
| F18 | `model_is_retired` 改成参考口径 `status ?? lifecycle`；写侧 `validate_model_filter` 用 `eq_ignore_ascii_case` | 与读侧/参考同口径 |
| F19 | `describe_changes` 的字段清单补 `modelFilter` | 既有清单 |

---

## 2. 实施顺序（按风险 × 依赖）

1. **批次 1（崩溃与功能失效）**：F1 → F2 → F3 → F4。这四条各自独立、互不依赖，改完即可 `cargo test`。
2. **批次 2（健壮性）**：F5 → F6 → F7（含 §4 的取舍）。
3. **批次 3（清理与口径）**：F8、F9、F10–F19（多为单行）。
4. 每批结束跑一次验证；**全程落 beta 分支，不推 main、不打 tag**（`agent.md` 红线）。

## 3. 验证清单

```bash
cd desktop-tauri/src-tauri/server && cargo test --lib     # 基线 630 passed；本方案应新增 ≥10 条
cd desktop-tauri/ui-islands && npm run build              # 本轮不改前端，作回归确认
cd desktop-tauri/i18n && node scan.mjs --check             # 同上
```
- 逐项单测见各条「验证」；**F1 的用例必须断言「不 panic」**（这是本方案唯一的安全项）。
- 回归重点：F8 的短路不得改变非 systemone 家的行为；F18 的口径变化不得改变现网退役判定（现网目录未见 `status` 与 `lifecycle` 冲突）。

## 4. 需要你拍板的两个取舍

1. **F7**：只做「内置白名单 + 快照表 + 注释（≈5 行）」，还是连「按目录 `provider.npm` 判协议（≈+10 行，与参考同构）」一起做？
   → 我建议**两者都做**（参考实现就是这么判的，且能自动跟上新增的 systemone 免费模型）。
2. **F9**：走 a（接线到已写好的验身份路径，净删一个死函数）还是 b（只改注释 + 删死代码）？
   → 我建议 **a**（与参考实现一致，且能避免「导入一枚废令牌后一用就 401」）。

## 5. 明确不做（避免过度工程）

- 不新增任何 crate（`flate2` / `sha2` / `base64` / `getrandom` 均已具备）。
- 不新建模块 / 不改 `ProviderAdapter` 契约 / 不改 `UpstreamResponse`。
- 不引入「按模型协议路由」（方案 §B3 已定为有意差异）。
- F10 不顺手去实现「免费标记出到 `/v1/models`」——那需要动聚合层 `list_item` 的字段契约，超出本次修复范围。
