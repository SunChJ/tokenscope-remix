# TokenScope 多智能体用量设计方案（Multi-Agent Design）

> 版本：v0.1 草案 · 2026-07-02
> 目标：从「Claude 用量仪表盘」演进为「本地所有 AI Agent 的统一用量仪表盘」。
> 第一步：支持 Codex CLI。

---

## 1. 产品定位升级

### 1.1 新的一句话定位

> 一个常驻菜单栏的小工具，统一展示本机所有 AI 编码工具（Claude Code、Codex、未来更多）的 Token 用量、花费与使用习惯。

### 1.2 为什么是这个方向

- 开发者普遍**同时使用多个 CLI Agent**（Claude Code + Codex 双持很常见），但每个工具的用量入口互相割裂。
- 各工具都把会话日志落在本地（`~/.claude`、`~/.codex`…），零侵入解析的技术路线可以完全复用。
- 「跨 Agent 对比」本身就是新价值：这周 Claude 和 Codex 各烧了多少？哪个项目在用哪个工具？

### 1.3 核心原则（继承自现有 PRD，不变）

1. 零侵入：只读日志，不装 hook、不代理流量。
2. 全本地：不上传任何数据。
3. 花费永远标注「估算 (est.)」。

---

## 2. 数据源抽象：Source Adapter

这是本次架构的核心：把「Claude」从硬编码变成第一个 **UsageSource 适配器**。

```
trait UsageSource {
    fn id() -> "claude" | "codex" | ...     // 稳定标识
    fn display_name() -> "Claude Code" | "Codex"
    fn detect() -> bool                      // 本机是否安装/有数据
    fn watch_paths() -> Vec<PathBuf>         // FS 监听目录
    fn parse_increment(file, offset) -> Vec<UsageEvent>  // 增量解析
}
```

### 2.1 统一事件模型（Normalized Schema）

所有适配器解析后归一化为同一结构，聚合层/UI 层完全 source-agnostic：

```jsonc
{
  "source": "codex",              // 归属 Agent
  "sessionId": "...",             // 会话去重
  "eventId": "...",               // 消息/回合去重
  "timestamp": "...",
  "model": "gpt-5.5",
  "tokens": {
    "input": 470,                  // 非缓存输入（已扣除 cached）
    "output": 274,
    "cacheRead": 78208,
    "cacheWrite": 0,               // Codex 无此概念，置 0
    "reasoning": 0                 // Claude 无此字段，置 0（含在 output 内）
  },
  "cwd": "/Users/.../project",
  "toolCalls": [{ "kind": "mcp", "server": "...", "name": "..." }]
}
```

### 2.2 Claude 适配器（现状迁移）

现有 parser.rs 逻辑原样搬进适配器，无行为变化。

### 2.3 Codex 适配器（本期新增）

**数据源**：`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`
（目录可被 `CODEX_HOME` 环境变量重定向，设置里允许自定义路径。）

关键事件与字段（已在本机实测验证，codex-cli 0.142）：

| 事件 | 用途 | 关键字段 |
|------|------|---------|
| `session_meta` | 会话元信息 | `session_id`、`cwd`、`cli_version`、`model_provider`、`source.subagent`（子代理标记）、`parent_thread_id` |
| `turn_context` | 回合上下文 | `turn_id`、`model`（如 `gpt-5.5`）、`cwd`、`reasoning_effort` |
| `event_msg / token_count` | **Token 用量** | `info.last_token_usage`（本回合增量）与 `info.total_token_usage`（会话累计）：`input_tokens` / `cached_input_tokens` / `output_tokens` / `reasoning_output_tokens` |
| `event_msg / token_count` | **配额** | `rate_limits.primary`（5h 窗口 used_percent / resets_at）、`rate_limits.secondary`（周窗口）、`plan_type` |
| `response_item / function_call` | 工具调用 | MCP / 自定义工具调用统计 |

**解析要点**：

1. **Token 口径归一**：Codex 的 `cached_input_tokens` 是 `input_tokens` 的子集（Claude 的 cache_read 则是独立字段）。归一化时 Codex 的 `input = input_tokens - cached_input_tokens`、`cacheRead = cached_input_tokens`，保证跨 Agent 口径一致。
2. **按回合取增量**：用 `last_token_usage` 作为单事件用量，按时间戳归入天/小时桶；模型归属取最近一条 `turn_context.model`（同一会话可中途换模型）。
3. **去重**：`token_count` 无消息 id，用 `(file, 行号/字节偏移)` 作为 eventId；会话被 resume/fork 时是新文件新 session_id，`parent_thread_id` 仅作参考，不合并。
4. **子代理**：`source.subagent`（如 review）的会话照常计入 token，会话数统计时归入主线程（可后期再细化）。
5. **兜底校验**：每个文件最终的 `total_token_usage` 可用来校验增量求和是否漏算（容错：坏行跳过）。
6. **花费**：`model` + `model_provider` 映射 LiteLLM key（`gpt-5.5` 等 OpenAI 系模型价格表已覆盖）；缓存部分按 `cache_read_input_token_cost` 计价。

### 2.4 未来适配器（占位，不实现）

Gemini CLI、Cursor CLI、OpenCode、Copilot CLI……只要实现 UsageSource 即插入。UI 按「已检测到数据的 source」动态展示，代码里不出现写死的双 Agent 假设。

---

## 3. 视觉与交互方案

### 3.1 总体思路：一套面板，一个过滤器，一套颜色语言

不做多页签、不做双栏对比视图。在现有单列面板上加一个 **Agent 过滤器**，并引入 **按 Agent 着色** 的颜色语言。理由：

- 现有面板信息架构（总量 → 趋势 → 模型 → 花费 → 工具 → 热力图）对任何 Agent 都成立；
- 菜单栏浮窗宽度有限，双栏对比放不下也没必要；
- 「All 聚合视图」才是日常主视图，单 Agent 视图是下钻。

### 3.2 Agent 颜色语言

每个 Agent 分配一个固定的品牌色调，贯穿全部图表（堆叠条、圆环、列表圆点、过滤 chip）：

| Agent | 色调 | 示例 |
|-------|------|------|
| Claude Code | 珊瑚橙 `#D97757`（Anthropic 品牌色系） | 深浅两档用于 input/output |
| Codex | 青绿 `#10A37F`（OpenAI 品牌色系） | 同上 |
| All（聚合） | 沿用现有绿色主题 | 中性 |
| 未来 Agent | 从预置调色板顺序取（蓝/紫/黄…） | — |

交互细节：**过滤到单个 Agent 时，整个面板的强调色切换为该 Agent 的色调**（图表、进度条、菜单栏图标 tint 不变）。用户一眼就知道当前在看谁。深浅色主题各配一组通过对比度校验的色值。

### 3.3 面板布局（All 视图）

```
┌──────────────────────────────────────────┐
│ ◉ Tokenscope            [Day|Week|Month] │  ← 不变
│ ┌─────┬────────┬───────┐                 │
│ │ All │ ⬤Claude│ ⬤Codex│   ← Agent 过滤 chips（新增，仅检测到≥2个源时显示）
│ └─────┴────────┴───────┘                 │
│ TOTAL TOKENS                    Est.cost │
│ 12.40M ▲14%                      $46.10  │  ← 全 Agent 合计
│ ⬤ Claude 8.1M        ⬤ Codex 4.3M       │  ← 原 Input/Output 行改为按 Agent 分段条
│                                          │
│ ▐▐▐▐▐▐▐  周趋势堆叠柱状图                │  ← 堆叠维度改为 Agent（橙+青绿）
│                                          │     hover 提示各 Agent 当日量
│ TOKENS BY MODEL                          │
│ ⬤ Claude Sonnet 4.5   ████████ 5.8M 47%  │  ← 圆点用所属 Agent 色
│ ⬤ GPT-5.5             ███      1.9M 16%  │
│                                          │
│ COST BY MODEL   ◔ $46.10                 │  ← 圆环切片按 Agent 色系分深浅
│                                          │
│ [REQUESTS 2,847] [COST TREND $46.10]     │  ← 不变，数据为合计
│                                          │
│ MCP CALLS            1,284 · 14 servers  │  ← 两源合并，行尾小圆点标 Agent
│ SKILL CALLS （Claude 专属，见 3.5）       │
│ DAILY ACTIVITY 热力图（合计）             │
└──────────────────────────────────────────┘
```

要点：
- **Input/Output 分段条 → Agent 分段条**：All 视图下最重要的问题从「输入输出比」变成「谁在烧」。Input/Output 拆分下沉到单 Agent 视图。
- 只装了一个 Agent 的用户**看不到任何变化**：chips 不出现，面板与今天完全一致（着色沿用现有绿色）。

### 3.4 单 Agent 视图（点击 chip 下钻）

- 布局与现在的 Claude 视图一致：总量条恢复为 **Input / Output** 拆分，趋势图恢复 input/output 堆叠。
- 强调色切换为该 Agent 色调。
- **Codex 视图专属卡片：配额（Rate Limits）** —— 数据直接来自日志，Claude 没有的差异化能力：

```
┌ CODEX QUOTA ────────────────────────────┐
│ 5h window    ████████░░░░░░  62%        │
│ Weekly       ███░░░░░░░░░░░  23%        │
│ Plan: Pro          resets in 2h 14m     │
└─────────────────────────────────────────┘
```

  取**最新一条** `token_count.rate_limits`；数据超过 1 小时未更新时置灰并标注「as of HH:MM」。≥80% 时进度条转警示色，可选系统通知（后期）。
- Codex 视图中 output 条内再细分 **reasoning tokens**（浅色纹理段），hover 显示占比。

### 3.5 各区块在不同过滤态下的行为

| 区块 | All | Claude | Codex |
|------|-----|--------|-------|
| 总量卡 | 合计 + Agent 分段条 | Input/Output 条 | Input/Output 条（含 reasoning 细分） |
| 趋势柱状图 | 按 Agent 堆叠 | 按 In/Out 堆叠 | 按 In/Out 堆叠 |
| By Model | 全部模型，圆点带 Agent 色 | 仅 claude-* | 仅该源模型 |
| Cost 圆环 | 按模型，色相随 Agent | 现状 | 同左 |
| MCP Calls | 合并 + 行尾 Agent 圆点 | 仅 Claude | 仅 Codex |
| Skill Calls | 显示（仅 Claude 数据） | 显示 | **隐藏**（Codex 无此概念） |
| Quota 卡 | 隐藏 | 隐藏 | **显示** |
| 热力图 | 合计 | 单源 | 单源 |

原则：**没有数据的区块整块隐藏，不显示空表**；概念不存在的功能不硬造对应物。

### 3.6 菜单栏

- 数字 = **所有 Agent 今日合计**（延续「只显示 token 不显示钱」）。
- 设置项「菜单栏统计范围」：All（默认）/ 仅某个 Agent。
- 图标不按 Agent 换色，保持系统级中性。

### 3.7 空态与引导

- 首次启动自动探测：`~/.claude/projects` 与 `~/.codex/sessions` 谁有数据就启用谁。
- 检测到新数据源时，面板顶部出现一次性提示条：「检测到 Codex 用量数据，已自动纳入统计 ✕」。
- 设置页：每个数据源一行 —— 开关 + 路径（默认路径 / 自定义）+ 检测状态（`已检测到 N 个会话` / `未找到数据`）。

---

## 4. 分期计划

### Phase 1 — Codex 支持（本期）
1. Rust 端抽出 `UsageSource` trait，Claude 逻辑迁入适配器（行为不变，回归验证）。
2. Codex 适配器：目录扫描 + FS 监听 + token_count/turn_context 增量解析 + LiteLLM 计价。
3. UI：Agent 过滤 chips、Agent 颜色语言、All 视图分段条/堆叠图、单源视图。
4. Codex Quota 卡片。
5. 设置页数据源管理 + 空态。

### Phase 2 — 打磨
- MCP 合并视图的 Agent 标注；reasoning token 细分；配额 ≥80% 通知。
- 「按项目 (cwd)」切片：两源都有 cwd，天然可做跨 Agent 项目视图。

### Phase 3 — 更多 Agent
- Gemini CLI / Cursor / OpenCode / Copilot CLI 适配器，chips 超过 4 个时折叠为下拉。

---

## 5. 关键决策记录

1. **过滤器而非页签/双栏**：All 聚合是主场景，单源是下钻；浮窗宽度也不允许并排对比。
2. **All 视图堆叠维度从 In/Out 改为 Agent**：多源场景下「谁在烧」比「输入输出比」更重要，后者下沉到单源视图。
3. **Codex 用 last_token_usage 增量而非 total 快照**：才能落到小时/天粒度桶；total 仅作校验。
4. **单源用户零变化**：只有检测到 ≥2 个数据源才出现任何多 Agent UI，避免打扰存量用户。
5. **Quota 卡做成 source 专属能力**：适配器可声明可选能力（capabilities），UI 按能力渲染，未来别的 Agent 有独有数据同理接入。
