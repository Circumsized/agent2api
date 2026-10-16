# Agent2API：引进 Devin 内置 provider + 修正 OpenCode Zen 免费模型清单

> 参考实现：`yetone/magpie`（本地 AI 网关，MIT）与 `magpie-community/plugins`（MIT）。
> 本文是**方案 + 落地方案**；用户已确认「方案文档 + 代码实现」为交付形态。
> 说明：用户最初给的 `https://github.com/Blinue/Magpie/` 是**窗口放大工具**（WinUI/DirectX），
> 与模型反代无关；经确认，实际参考对象为 `yetone/magpie` / `usemagpie.ai`。

---

## 0. Summary

本次两项改动，彼此独立：

| 项 | 目标 | 现状 |
|---|---|---|
| **A. Devin 内置 provider** | 让 agent2api 能把 Devin 订阅（devin CLI 的账号）的模型反代成一个 OpenAI 兼容 API | **完全缺失**（全仓 `devin` 零命中） |
| **B. OpenCode Zen 模型清单** | 只广告**真正可用的免费**模型，剔掉抓进来的一堆付费模型与垃圾条目 | 已能把 Zen 反代成 API（自定义 provider + 官方客户端伪装），但取「模型清单」时**不过滤**，把付费模型也列出来了 |

`CodeArts` 已是完整内置 provider，无需改动；`OpenCode Zen` 的**转发**链路已成熟（伪装逻辑见
`providers/custom/emulation.rs`），本次只修**模型清单**这一处。

---

## 1. Current State Analysis（基于实际代码与上游实测）

### 1.1 参考实现是怎么做的

**`magpie-community/plugins/packages/devin`** —— Devin **不是一个 agent 目标，而是一个
subscription provider**（provider id `devin`，声明为 `@ai-sdk/openai-compatible`）。
它把 OpenCode 的 chat completion **整体**送进「CLI 真正在用的那个 API」——Windsurf 的
`GetChatMessage`（一个 protobuf 的 Connect RPC，跑在 `server.codeium.com`），把回包流式翻译回
chat completion chunk。核心模块与常量（逐字摘自其 `index.mjs`）：

```
SERVER       = https://server.codeium.com
AUTHORIZE    = https://app.devin.ai/auth/cli/continue          // PKCE 授权页
EXCHANGE     = /exa.seat_management_pb.SeatManagementService/ExchangeDevinCLIPKCECode
CHAT_RPC     = /exa.api_server_pb.ApiServerService/GetChatMessage
MODEL_CONFIGS_RPC = /exa.api_server_pb.ApiServerService/GetCliModelConfigs
CLI_VERSION  = 3000.11.3
```

- **凭证**：`Authorization: Basic <token>`；token 形如 `devin-session-token$<jwt>`，两条来源：
  1. **浏览器 PKCE**：`app.devin.ai/auth/cli/continue`（`code_challenge_method=S256`、
     `cli_pkce_marker=1`），回跳 `http://127.0.0.1:<port>/callback`，再拿 code 去
     `server.codeium.com` 换 token；
  2. **复用 CLI 登录态**：读 `$XDG_DATA_HOME/devin/credentials.toml`（Windows `%APPDATA%\devin\credentials.toml`），
     键 `windsurf_api_key` / `api_server_url` / `devin_webapp_host` / `devin_api_url`，**只读不改**。
  token 不定期过期，无刷新；被拒即 401。
- **传输**：Connect RPC = HTTP POST + `Content-Type: application/proto`（响应
  `application/connect+proto`），**protobuf 手写**（一个小 `PB` 写入器 + 一个 `fields()` 读取器）。
  流式帧 = 5 字节前缀（bit0=gzip、bit1=结束）+ 消息体。
- **模型目录**：`GetCliModelConfigs` 的 protobuf 响应，已知字段号：
  - `client_model_configs`(1) → `model_uid`(22)、`supports_images`(5)、`model_cost_tier`(24)、
    `prices`(32，内含 label(1)/float32(2)/unit(3))、`label`(1)、`max_context_tokens`(18)、
    `model_info`(23){`output`(13)、`family uid`(23)}、`family_metadata`(30){`label`(1)}。
  - **免费判据**：`model_cost_tier == 4`（FREE_TIER）。
  - 目录是「family（如 `swe-2`/`claude-opus-5-5`）+ 各 effort 的 variant（如 `swe-2-high`）」，
    对外只列 family 一次（附 effort 与 `-fast`/`-priority` 快跑），用户点名时用 `variantFor`
    折成具体 variant id 发给上游。`Adaptive`/`Fusion` 不列（它们在 Devin 内部路由，不经 API）。
- **请求改写**（上游实测的硬约束）：system 提示词并进**首条 user 消息**头部；每个工具的
  description 摘进一个 `<tool_descriptions>` 块；会话里**未作答的 tool call** 补一条
  「Tool use was interrupted and did not produce a result.」；图片**必须是内联 `data:` URL**；
  工具 schema 要按家族折叠（Claude 家族禁止根级 anyOf/oneOf/allOf，Gemini 家族禁止
  类型是数组等）。
- **错误映射**：token 被拒 401、限额 429、超长 400。
- 明确**不做**：usage/额度、多账号、web 搜索。

**`magpie-community/plugins/packages/zen-free`** —— Zen **免费**模型的取法与过滤规则
（其 `models.mjs`，逐字）：

```js
export function isFree(id, model = {}) {
  return (
    !isRetired(model) && (/free/i.test(id) || (model.cost?.input === 0 && model.cost?.output === 0))
  );
}
// buildModels：对 GET {base}/models 的每个 id：
//   if (!isFree(id, model) || !Object.hasOwn(SDK, protocol)) continue;
```

数据源三份（`Promise.allSettled`）：
1. `GET https://opencode.ai/inference/v1/models`（id 列表；`authorization: Bearer public`，
   `x-opencode-client: cli`）；
2. `https://models.opencode.ai/api.json` → `.opencode.models`（含 `cost`，用于 0 代价判据；
   也是 `big-pickle` 被认定为免费的依据）；
3. OpenCode console 文档 `models.mdx`（每模型的协议表），失败时退回内置
   `bootstrapProtocols` / `bootstrapProvider`。

`SDK` 只含 `chat`/`responses`/`anthropic` 三种 npm，因此 **SystemOne** 协议（如
`jev-1.13-free`）被 `!Object.hasOwn(SDK, protocol)` 挡掉 —— 这就是「Jev 是 SystemOne 决策模型、
不是拿来聊天的」。

### 1.2 agent2api 现状（本仓）

- OpenCode Zen 预置卡：[preset-providers.js:206-217](file:///workspace/desktop-tauri/ui/preset-providers.js#L206-L217)
  ```js
  { key: 'opencode-zen', protocol: P.openai, baseUrl: 'https://opencode.ai/inference/v1',
    clientEmulation: 'opencode', account: { noAuth: true } }   // 无 modelFilter
  ```
- 取模型清单：[custom_providers.rs:943-1019](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/custom_providers.rs#L943-L1019)
  `fetch_upstream_models` —— 直接 `GET {baseUrl}/models`，解析 `data[].id`，**零过滤**返回。
  （自定义 provider 的模型以 `set_models` 落盘，`public_models` 读回广告；**没有**后台远程刷新，
  所以「取清单」这一步是唯一插入点。）
- 上游实测：`GET https://opencode.ai/inference/v1/models` 返回 **90 条**，其中免费为
  `–free` 结尾的 10 条（`ling-3.1-flash-free`、`cat-jam-free`、`mimo-v2.6-flash-free`、
  `muse-spark-1.2/1.3-contributor-free`、`nemotron-3.5-lightning-free`、`jev-1.13-free`、
  `step-5-preview-free`、`longcat-2.5-preview-free`、`space-bunny-free`）**加 `big-pickle`**
  （无 `-free` 后缀），其余 `claude-opus-5` / `gpt-5.5` / `kimi-k3` … 全是付费，另有垃圾 `test`。
  → 与用户反馈完全一致。
- 转发伪装（不动）：[emulation.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/custom/emulation.rs)。

### 1.3 新增「内置 provider」的接线面（以 CodeArts 为模板）

- 身份与元数据（**唯一事实来源**）：[providers/mod.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/mod.rs#L173-L472) `ProviderKind`
  + [PROVIDERS:495-568](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/mod.rs#L495-L568)
  + [kind_from_id:623](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/mod.rs#L623) + [kind_id:668](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/mod.rs#L668)。
- 适配器契约：[adapter.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs) `ProviderAdapter`（`kind`/`list_models`/
  `advertise_models`/`build_chat_request`/`is_stateful`/`forward_conversation`），
  [adapter_for:1096](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs#L1096)，
  有状态家的分流见 [adapter.rs:56-63](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs#L56-L63)
  与 [is_stateful:865](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs#L865) /
  [forward_conversation:890](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs#L890)。
  CodeArts 接在 [adapter.rs:1112](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs#L1112)。
  **「上游不是 JSON body 的自有协议」这一类（CatPaw/Qoder/Accio/CodeArts/Trae/Kuku/MonkeyCode）
  一律 `is_stateful=true` 走 `forward_conversation`，适配器自己完成发送与翻译。Devin 属于这一类。**
- 账号存储：[account_store/codearts_accounts.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/account_store/codearts_accounts.rs#L33)
  （`CODEARTS_PROVIDER_ID = kind_id(ProviderKind::CodeArts)`；`add_*_account` /
  `*_account_record` / `update_*_credentials_if_current` / `to_*_public_account`）。
- 登录：[core/login.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/login.rs) 分派 + `core/login/<provider>.rs`
  （`core/login/codearts.rs` 等 8 家）。
- 添加账号分派：[api/accounts.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/api/accounts.rs#L465-L887) 的 `match ... ProviderKind`（逐家一支）。
- UI：`ui/preset-providers.js`（自定义家）、[add-account-configs.ts](file:///workspace/desktop-tauri/ui-islands/src/islands/add-account-configs.ts#L362-L414)
  （内置家登录配置）、[add-provider-pick.tsx:75](file:///workspace/desktop-tauri/ui-islands/src/islands/add-provider-pick.tsx#L75)（图标）、
  [accounts-domain.ts:179](file:///workspace/desktop-tauri/ui-islands/src/islands/accounts-domain.ts#L179)（展示）。
- i18n：`desktop-tauri/i18n/i18n-src/{en,ja,ko,pt-BR,zh-Hant}/{add-account,accounts}.json`
  （**zh-Hant 是空串待译**，沿用现状）。
- 依赖：server crate **没有** protobuf/gRPC crate；已有 `flate2`（gzip）、`sha2`、`base64`、
  `getrandom`、`toml`、`reqwest`、`tokio`、`serde_json` —— Devin 所需的原语**全部已具备**，
  **不新增任何依赖**。

---

## 2. Proposed Changes

### A. 新增 Devin 内置 provider

#### A1. 新模块 `server/core/providers/devin/`

| 文件 | 职责 / 关键内容 |
|---|---|
| `mod.rs` | 模块头（写清「反代谁、凭什么、与各家同款有状态路由」）；`pub static DEVIN_ADAPTER: DevinAdapter`；`ProviderAdapter` 实现：`kind`、`list_models`、`is_stateful=true`、`forward_conversation`、`advertise_models`（默认恒等） |
| `endpoints.rs` | 常量：`SERVER`、`AUTHORIZE`、`EXCHANGE`、`CHAT_RPC`、`MODEL_CONFIGS_RPC`、`CLI_VERSION`、`os_name()`；`chat_url()` / `model_configs_url()` / `exchange_url()`；`api_server_url` 可被账号上的 `api_server_url` 覆盖（同 magpie：`WINDSURF_API_SERVER_URL` 语义） |
| `pb.rs` | **手写 protobuf**：`Pb`（`varint`/`bytes`/`str`/`double`/`done`）、`fields(&[u8])`（wire 0/1/2/5）、`Connect` 帧读取器（5 字节前缀，bit0 gzip → 复用现有 `flate2`，bit1 = 流结束）。**不引入新依赖** |
| `credentials.rs` | `Credential { session_token, api_server_url, webapp_host, devin_api_url }`；`to_record_value()`（落盘投影，供 `account_store` 与「能不能用」判定）；`parse_credentials_toml()`（极简 TOML 行解析）；`cli_credentials_path()`（`$XDG_DATA_HOME\|~/.local/share` 或 Windows `%APPDATA%`） |
| `oauth.rs` | 浏览器 PKCE：生成 `verifier`/`challenge`（`sha2`+`base64`+`getrandom`，S256）、拼 `AUTHORIZE` 参数（含 `cli_pkce_marker=1`）、回调接收、`ExchangeDevinCLIPKCECode` 换取 token |
| `models.rs` | `SNAPSHOT`（family 表，移植 magpie `index.mjs:44-99`）；`parse_model_configs` / `parse_free`（tier==4）/ `parse_prices` / `parse_config_families`；`listed_families()`（family 折叠 + effort + fast 变体）；`variant_for(family, model, effort)`；`NO_IMAGES` 与 `snapshot_images` |
| `chat.rs` | `build_get_chat_message(chat_body, model_uid, token) -> Vec<u8>`：variant 选择；system → 首条 user 头；`<tool_descriptions>` 块；未作答 tool call 补 `NO_RESULT`；`object_root()` / `gemini_schema()` 折叠；图片内联 `data:` URL 校验；`metadata(key)`（`str(1,"devin-cli").str(2,CLI_VERSION).str(3,token)…`）；`out_id`/`in_id`（`dv_`+base64url，绕开 Claude Code 不认 `:`/`#` 的 tool id） |
| `stream.rs` | Connect 帧 → `chat.completion.chunk` 及思考/工具调用/usage；HTTP 状态与流内错误 → 401/429/400（复用现有 `GatewayError` 分类） |

#### A2. 接线（按 1.3 的模板，逐处）

1. [providers/mod.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/mod.rs)：`ProviderKind::Devin` 变体（**加在末尾**）；`PROVIDERS` 追加 `ProviderMeta { id: "devin", label: "Devin" }`；`kind_from_id` 加 `"devin"`；`kind_id` 加 `ProviderKind::Devin => "devin"`；`pub mod devin;`。
2. [adapter.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/providers/adapter.rs)：`adapter_for` 加 `ProviderKind::Devin => &super::devin::DEVIN_ADAPTER`。
3. `core/account_store/devin_accounts.rs`（新）+ [account_store/mod.rs:107](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/account_store/mod.rs#L107) 注册 `pub mod devin_accounts;`：`DEVIN_PROVIDER_ID = kind_id(ProviderKind::Devin)`、`add_devin_account`、`devin_account_record`、`update_devin_credentials_if_current`、`to_devin_public_account`（字段名以 `credentials.rs::to_record_value` 为准）。
4. `core/login/devin.rs`（新）+ `core/login.rs` 分派：两种登录 —— 浏览器 PKCE、导入本机 CLI `credentials.toml`（只读）。
5. [api/accounts.rs](file:///workspace/desktop-tauri/src-tauri/server/src/server/api/accounts.rs#L465-L887)：加 `ProviderKind::Devin` 分支（粘贴 token / 走登录流程）。
6. UI：`add-provider-pick.tsx` 图标 `assets/providers/devin.png`（新资产）；`add-account-configs.ts` 加 `devin` 登录配置（浏览器登录 + CLI 导入，两方法）；`accounts-domain.ts` 展示名。
7. i18n：五个语言包补 Devin 文案（en/ja/ko/pt-BR 写译文，zh-Hant 沿用空串惯例）。
8. 文档：`README.md`「支持的提供商」表加 Devin 行；`LICENSE` 的附加声明若涉及移植，追加对 magpie / magpie-community 的 MIT 署名（见 §3 决策 D3）。

**不做（v1 明确划界）**：余额/额度查询（`accounts_usage`）、多账号并发、Devin 的 web 搜索替代 —— 与 magpie 插件一致；`supports_refresh = false`，token 无续期接口。

#### A3. 为什么走「参考插件源码」而不是「抓包逆向」

用户已确认：以 `magpie-community/plugins` 的**实现代码**为准。该插件是 magpie Go 内置实现的移植（其首行注释即写明 `Ported from magpie's built-in Devin account (internal/provider/devin*.go, internal/gateway/devin.go)`），
字段号、RPC 路径、请求改写规则都是**已在生产跑通**的常量 —— 据此移植即可，不需要在本机跑 CLI 抓包。

### B. 修正 OpenCode Zen 的模型清单

#### B1. 引入「模型清单过滤器」概念（挂在自定义 provider 记录上）

- 常量：`custom_providers::MODEL_FILTER_OPENCODE_FREE = "opencode-free"`（与 `CLIENT_EMULATION_OPENCODE` 同处定义，读写校验同源）。
- 记录字段：provider 记录新增可选键 `modelFilter`；把该键加入 `custom_providers` 的**已知字段归一化/读写白名单**（与 `clientEmulation`/`balance`/`quirks` 同层），使 `public_models`/`forward` 之外的创建链路能原样落盘。
- 预置卡：[preset-providers.js:206-217](file:///workspace/desktop-tauri/ui/preset-providers.js#L206-L217) 的 `opencode-zen` 增加 `modelFilter: 'opencode-free'`（**只动这一家**；Go 面、其它自定义家不受影响）。

#### B2. 过滤实现

在 [custom_providers.rs `fetch_upstream_models`:943-1019](file:///workspace/desktop-tauri/src-tauri/server/src/server/core/custom_providers.rs#L943-L1019) 的「解析 `data[].id`」之后（L1018 之前）加一步
`apply_model_filter(provider, ids)`，仅当 `modelFilter == "opencode-free"` 时生效，规则**逐字对齐 magpie**：

1. 尽力而为拉目录 `https://models.opencode.ai/api.json` → `.opencode.models`（15s 超时，**失败不报错**，退化到规则 2）。
2. 对每个 id：
   - **剔除 retried/retired**：目录里 `deprecated===true` 或 `status/lifecycle ∈ {deprecated,retired,disabled}` 或 `deprecated_at`/`retirement_date` 非空；
   - **保留**：`id` 命中 `/free/i`（不区分大小写）**或** 目录 `cost.input===0 && cost.output===0`（→ 这使
     `big-pickle` 被正确保留）；
   - **剔除 SystemOne**：协议为 `systemone` 的 id（例如 `jev-1.13-free`）。判据优先级：目录/文档给出的协议
     ∉ {chat, responses, anthropic} → 剔除；取不到协议时退回**内置排除表**（当前仅 `jev-1.13-free`）。
3. 目录取不到时（规则 2 退化）：仅按 `id 命中 /free/i` ∪ **内置免费白名单**（当前含 `big-pickle`）保留，
   并按内置排除表剔 SystemOne/垃圾（如 `test`）。

> 内置 `bootstrapProtocols` / 免费白名单 = 移植 magpie 的 `bootstrapProtocols` / `bootstrapProvider`
> 中与「判是否免费」相关的条目（`models.mjs` 尾部的两张表）。

#### B3. 不做的事

- **不改**转发链路与伪装逻辑（`emulation.rs` 一行不动）—— 本次只收窄「取清单」结果。
- 自定义 provider **没有后台远程刷新**（模型以 `set_models` 落盘、`public_models` 读回广告），
  因此**唯一插入点**就是 `fetch_upstream_models`；已在档的旧清单需用户重取一次（见 §4 验证）。
- 不同步搬 magpie 的「每模型一套协议（chat/responses/anthropic 分别路由）」。agent2api 的自定义
  provider 是**单一协议**（此处 `openai`/chat）。因此协议面只用来**排除**不能聊的（SystemOne），
  responses-only 的免费模型（如 `muse-spark-1.3-contributor-free`）**仍然列出**（转发时按既有 chat 路径发，
  是否可用由上游决定）—— 这是与 magpie 的**有意差异**，写在模块头里。

---

## 3. Assumptions & Decisions

- **D1（路线）**：Devin 走 **Connect-RPC / `GetChatMessage` 复刻**，schema 依据来自
  `magpie-community/plugins/packages/devin`（用户已确认）。
- **D2（架构定位）**：Devin = **有状态适配器**（`is_stateful=true` + `forward_conversation`），
  与 CodeArts/Trae 同款；**不新增** `UpstreamResponse` 变体。
- **D3（许可）**：移植自 MIT 项目，保留「逐处注明来源」的模块头注释；在 `LICENSE` 附加声明里补一句
  对 `yetone/magpie` / `magpie-community/plugins` 的致谢与许可引用。
- **D4（不做）**：Devin 余额/多账号/web 搜索；OpenCode 每模型协议路由。
- **D5（依赖）**：protobuf 手写，**零新增依赖**。
- **D6（风险）**：Devin 协议未公开、随上游变更可能失效；本沙箱**无法端到端验证**（无 Devin 账号/CLI）。
  实现以「与插件源码逐字段对齐」为正确性口径，真实联调需用户提供账号（见 §4）。
- **D7（i18n）**：沿用现状 —— en/ja/ko/pt-BR 给译文，zh-Hant 留空串。
- **D8（分支）**：按 `agent.md`，日常改动一律落 `beta`；**不碰 `main`、不推 tag**。

---

## 4. Verification

### 通用（每次改动后）

```bash
cd desktop-tauri/src-tauri/server && cargo check     # 编译期穷举：新 kind 必须给出 adapter_for 分支
cd desktop-tauri/src-tauri/server && cargo test       # 含各 provider 的路由/翻译单测
```

### A. Devin

1. **单测**（新增于 `providers/devin/`）：
   - `pb.rs`：`fields()` 对已知字节序列的解包（wire 0/1/2/5）、gzip 帧。
   - `models.rs`：用**插件里那段 `parseModelConfigs`/`parseFree`/`parsePrices` 的同构样例**（或
     手工构造的 protobuf）断言 family 折叠、effort、`free`（tier==4）。
   - `chat.rs`：system 并入首条 user、`<tool_descriptions>` 块、未作答 tool call 补 `NO_RESULT`、
     非 data: 图片被拒、`out_id`/`in_id` 往返。
   - `stream.rs`：帧 → chunk 的文本/思考/工具/usage 映射；401/429/400 分类。
2. **端到端（需用户配合）**：用真实 Devin 账号（`devin auth login`）在本机添加账号 →
   `GET /v1/models` 应列出 family 列表 → 发一次 `/v1/chat/completions`（含一次工具调用）确认
   流式与工具往返。真实 token 与账号由用户提供/在本机执行，代理不代持凭证。
3. 回归：确认未影响其它 provider 的路由与 `/v1/models` 合并。

### B. OpenCode Zen

1. **单测**：对 §1.2 那份**真实 90 条 id 响应**做断言 —— 过滤后应恰为
   `big-pickle` + 9~10 条 `-free`（不含 `jev-1.13-free`、`test`、全部付费模型）；
   目录拉取失败时的退化分支同样断言。
2. **手动**：添加「OpenCode Zen」账号 → 「获取模型」→ 弹出的候选里**只剩免费项**；
   保存后 `/v1/models` 里 Zen 只出现免费项；对一条免费模型发一次对话确认可用。
3. **回归**：OpenCode **Go** 面与其它自定义 provider 的清单**不受影响**（过滤器只对
   `modelFilter == "opencode-free"` 生效）。

---

## 5. 实施顺序（建议）

1. **B 先做**（改动面小、可独立验证）：`modelFilter` 字段 + 过滤函数 + 预置卡 + 单测。
2. **A 后做**（改动面大）：`pb.rs`/`endpoints.rs` → `models.rs` → `chat.rs` → `stream.rs` →
   接线（ProviderKind→PROVIDERS→kind_from_id/kind_id→adapter_for→account_store→login→api）→
   UI/i18n/README。
3. 全程落 **beta**，不推 main、不打 tag。
