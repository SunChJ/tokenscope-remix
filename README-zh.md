# Tokenscope

[English](README.md) · **中文**

**macOS 菜单栏 / Windows 系统托盘工具**，为本机 **AI 编码 Agent**（Pi、Claude Code 与 Codex CLI）提供一个统一仪表盘：**每日 Token 用量、估算花费、按模型 / MCP / Skill 的调用统计、可靠性遥测，以及实时订阅额度**。

技术栈：**Tauri 2 + React + TypeScript**（前端）/ **Rust**（数据层）。

![Tokenscope 面板（深色 / 浅色）](docs/screenshot.png)

## 安装

### macOS：Homebrew（推荐）

```bash
brew install --cask sunchj/tokenscope/tokenscope
```

cask 的 `postflight` 已内置 `xattr -cr`，首次直接打开即可，不会弹「Apple 无法验证」。打开一次后注册为登录项，之后每次开机自动在菜单栏运行。

升级：`brew update && brew upgrade --cask tokenscope`

### macOS：下载 .dmg（备用）

1. 从 [Releases](https://github.com/SunChJ/tokenscope-remix/releases) 下载最新的 `Tokenscope_*_universal.dmg`
2. 拖入「应用程序」
3. 因为是**未签名 / 未公证**构建，首次打开会被 Gatekeeper 拦截——右键 →「打开」→ 再次确认，或执行 `xattr -cr /Applications/Tokenscope.app && open /Applications/Tokenscope.app`

> 未签名是当前的已知限制；要「双击直开」需 Apple Developer ID 签名 + 公证。

### Windows

1. 从 [Releases](https://github.com/SunChJ/tokenscope-remix/releases) 下载最新的 `Tokenscope_*_x64-setup.exe`
2. 按当前用户安装（无需管理员权限）。SmartScreen 首次会拦截——点 **"更多信息" → "仍要运行"**
3. 系统要求：**Windows 10 1803 及以上 / Windows 11**，需要 WebView2 运行时（Win 11 预装）

### 更新

App 启动时及此后每小时检测新版本，提示条位于左上角 **Tokenscope** 下方；也可用托盘菜单的 **Check for Updates…** 立即检查。Homebrew 用户继续使用 `brew upgrade --cask tokenscope`。

## 功能

- **Token 仪表盘**：今天 / 本周 / 本月 + 自定义日期区间（含首尾日）。检测到 ≥2 个 Agent 时出现过滤 chips（All / Claude / Codex / Pi）——All 视图**按 Agent** 堆叠用量，切到单 Agent 时整个面板换成其品牌色（Claude 珊瑚橙 / Codex 青绿 / Pi 紫）
- **三类切片**：**按模型**、**按 MCP 调用**、**按 Skill 调用**（只统计你自己安装的 server / skill，内置工具全部过滤）
- **项目结算**：按本地 Git 仓库 / 工作目录归集用量，导出 CSV，不暴露原始路径或对话正文
- **可靠性遥测**：完成/中止 Turn、工具错误/拒绝，以及中止任务浪费的 Token 与估算成本
- **Context 健康度**：上下文窗口中位/峰值压力、≥80% 提醒、压缩次数、reasoning Token 占比
- **额度详情**：**Claude**（5 小时 + 每周）与 **Codex**（每周 + Spark）的实时订阅额度，Codex status 风格展示——`29% left (resets 05:01 on 9 Aug)`，趋势点足够时给出每日消耗速率与预计触顶时间
- **菜单栏显示**：在菜单栏 Token 数后展示各 provider 最紧张窗口（`52.8M · Cl79% · Cx29%`）、完整窗口列表，或完全关闭
- 成本甜甜圈（hover 看单模型）、年度活跃热力图

## Provider 额度

订阅额度通过 **HappyUsage `hu` CLI** 获取，由它负责凭据发现、OAuth 刷新与 provider API 调用（Claude 走 Anthropic OAuth usage API，Codex 走 ChatGPT usage API）。

- `hu` 二进制**随应用包内置**（`Resources/bin/hu`），无需系统安装；每次发布由 `src-tauri/bin/build-hu.sh` 下载最新版并打进包内
- 运行时查找顺序：内置 `hu` → 系统安装的 `hu` → 24h 节流自动安装（brew tap → 官方脚本 → `go install`）→ 原生 Codex usage 调用（Pi / Codex CLI OAuth）→ 缓存快照
- 额度每 5 分钟在后台线程刷新，最近一次成功快照落盘 `provider_limits.json`，离线 / API 失败时仍可展示
- Claude 回退到 `~/.claude.json` 的 `cachedUsageUtilization`（Claude Code 运行时更新）；Codex 趋势合并 session 日志的 rate-limit 历史
- Tokenscope **从不缓存凭据**，只缓存额度元数据

菜单栏标签：**关闭**（`52.8M`）/ **紧凑**（`52.8M · Cl79% · Cx29%`，各 provider 最紧张窗口）/ **详细**（`52.8M · Cl 5h0/W79 · Cx W29/S31`）。右键托盘菜单另有 **额度详情** 子菜单，列出每个窗口的剩余百分比。

## 数据来源（本地优先；session 日志只读）

| 用途 | 路径 |
|------|------|
| Claude 会话日志（Token / 模型 / 工具调用） | `~/.claude/projects/**/*.jsonl` |
| Codex 会话日志（Token + 离线额度回退） | `~/.codex/sessions/**/*.jsonl`（支持 `CODEX_HOME`） |
| Pi 会话日志（Token / 模型 / 工具 / 遥测） | `~/.pi/agent/sessions/**/*.jsonl`（支持 `PI_CODING_AGENT_DIR`、`PI_CODING_AGENT_SESSION_DIR` 与绝对路径 `settings.sessionDir`） |
| Provider 额度（Claude + Codex） | 内置 HappyUsage `hu`（`Resources/bin/hu`）；回退：`~/.claude.json` 的 `cachedUsageUtilization`、基于 Pi / Codex CLI OAuth 的原生 usage API |
| Claude 用户 MCP 白名单 | `~/.claude.json` → `mcpServers` + `projects[*].mcpServers` |
| Codex 用户 MCP 白名单 | 全局及受信任项目 `config.toml` → `[mcp_servers.*]`（`$CODEX_HOME/config.toml`、项目 `.codex/config.toml`） |
| 用户 Skill 白名单 | Claude：`~/.claude/skills/`；Codex：`$CODEX_HOME/skills/`、`~/.agents/skills/` 与项目 `.agents/skills/`；Pi：全局、共享、项目级 Pi Skill 目录及 settings 显式路径 |
| 模型价格 | **主**：[models.dev](https://models.dev/api.json)（裸模型名，匹配 CLI 日志）→ **兜底**：[LiteLLM](https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json) → 内置快照。缓存于 `~/Library/Caches/tokenscope/`，24h 刷新 |

## 关键处理

- **Claude**：按 `message.id` 去重（流式/重试会重复 usage）；同一消息跨多行时合并其工具调用，token 只计一次
- **Codex**：由独立 adapter 将单次响应的精确 `last_token_usage` 与累计 `total_token_usage` 快照交叉校验，quota-only 重复事件和 fork 回放不会被当成新用量；持久化回放使用稳定的 `(turn, 累计快照)` ID 去重，并从包含缓存的 `input_tokens` 中拆出 `cached_input_tokens` / `cache_write_input_tokens`，形成可与 Claude 对比的四类统计口径
- **Pi**：每条持久化 assistant response 按其精确四类 `usage` 计入；用稳定的树 entry id 去重 `/fork` / `/clone` 复制的历史；日志中的请求成本、reasoning、stop reason、工具错误、compaction 与模型 context window 进入对应遥测
- **Token 拆分**：`input`（未缓存）/ `cache`（creation + read）/ `output`；UI 默认把 cache 并入 In 显示，并单列「cached %」
- **计价**：精确名 → 归一化名（去厂商前缀，`.`↔`p`，如 `glm-5.1`⇄`glm-5p1`）；models.dev 优先官方裸名价。Pi 有持久化请求成本时优先采用。查不到价格的模型仍计入 Token，UI 标注「暂无定价」
- **工具分类**：MCP 调用从 `mcp__<server>__*` 名称、Codex tool-search 映射或 Codex App 自定义调用归属 server，再按**所属 Agent** 配置过滤；Claude 的 Skill 调用与 Codex / Pi 的 `SKILL.md` 读取按 Agent 的用户/项目 skills 目录匹配；其余忽略

> 花费为按公开价格的**估算**；订阅用户应理解为「等效消费价值」。

### 四类 Token 与计价公式

每条 assistant 消息的 `usage` 给出四个**互斥**的 token 计数（同一 token 不会被重复统计）：

| 阶段 | `usage` 字段 | 含义 | 单价（相对 input） |
|------|-------------|------|------------------|
| **Input**（未缓存） | `input_tokens` | 本轮新发送的提示词 token | 1× |
| **Cache 写入** | `cache_creation_input_tokens` | 写入提示缓存的上下文 | 约 1.25× |
| **Cache 命中**（读） | `cache_read_input_tokens` | 从缓存重放的上下文 | 约 0.1× |
| **Output** | `output_tokens` | 模型生成的 token | 约 5× |

Pi 以 `usage.input`、`usage.cacheWrite`、`usage.cacheRead`、`usage.output` 持久化相同四类口径；可选的 `usage.reasoning` 已是 output 的子集，不会重复相加。

```
total = input + cache_creation + cache_read + output
# UI 展示： In = input + cache_creation + cache_read，  Out = output，  cached % = cache_read / total

cost = input            × price.input
     + cache_creation   × price.cache_creation
     + cache_read       × price.cache_read        # 缓存命中按折扣后的 read 单价计费
     + output           × price.output
```

缓存命中**不会**按普通 input 计费，而是用专门（更便宜）的 `cache_read` 单价——这就是重度缓存场景下 token 量很大、花费却不高的原因。UI 只是把 cache 折进「In」做展示，计费始终按四个独立单价。

## 开发

```bash
pnpm install
pnpm tauri dev         # 启动桌面 App（需要 Rust 工具链）
```

仅预览前端（用真实数据快照 `public/dev-dashboard.json`）：

```bash
pnpm dev               # http://localhost:1420
# 刷新快照：
cd src-tauri && cargo run --example dump > ../public/dev-dashboard.json
```

## 构建与发布

```bash
pnpm tauri build       # macOS：.app / .dmg；Windows：NSIS .exe → src-tauri/target/release/bundle/
```

构建时会自动下载并内置 `hu` 二进制（`src-tauri/bin/build-hu.sh`）。同步版本号后推送：

```bash
pnpm version:set 1.5.1
pnpm version:check
git commit -am "release: v1.5.1"
git push origin main
```

版本号变更推送到 `main` 后，GitHub Actions 会校验版本、创建 `vX.Y.Z` 标签，构建并发布 macOS / Windows 安装包、签名更新包与 `latest.json`（需配置 `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets）。

## 结构

```
src/                  React 前端
  data.ts             类型 + Tauri 桥 + 主题 + 格式化
  charts.tsx          图表原语（柱状/甜甜圈/sparkline/热力图/分段控件）
  App.tsx             主面板
src-tauri/src/
  store.rs            多源 JSONL 增量摄取（Claude + Codex + Pi 适配器）
  parser.rs           按 Scope 聚合（All / 单 Agent；预设周期 + 自定义区间 + 热力图）
  pricing.rs          models.dev / LiteLLM 价格加载与计价
  quota_api.rs        provider 额度：内置 HappyUsage `hu` + 原生 / 自动安装回退
  config.rs           用户 MCP / Skill 白名单（Claude + Codex + Pi）
  model.rs            返回给前端的数据结构
  lib.rs              Tauri 命令 + 菜单栏托盘
src-tauri/bin/        build-hu.sh——发布时下载内置的 hu 二进制
```

## Bug 记录

开发过程中遇到的典型 bug（现象、根因、解决办法）汇总在 [docs/BUGFIXES.md](docs/BUGFIXES.md)。

## 致谢

本项目源自 [@HduSy](https://github.com/HduSy) 的 [tokenscope](https://github.com/HduSy/tokenscope)——一个非常出色的 macOS 菜单栏 Claude CLI 用量仪表盘。本仓库继承的 Rust 数据摄取/聚合架构与面板设计均出自原作者之手，并沿用原项目的 MIT 协议，在此致以诚挚感谢！🙏

Tokenscope Remix 在其基础上扩展为多 Agent 仪表盘（见 [docs/DESIGN-multi-agent.md](docs/DESIGN-multi-agent.md)），并独立维护：问题与需求请在本仓库提交，不要打扰上游。Provider 额度部分使用 [HappyUsage](https://github.com/SunChJ/happyusage)（`hu`）负责凭据处理与 provider API 调用。
