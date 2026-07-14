# Tokenscope Remix

[English](README.md) · **中文**

**macOS 菜单栏 / Windows 系统托盘工具**，统一展示本机 **AI 编码 Agent**（Claude Code **与 Codex CLI**）的 **每日 Token 用量、估算花费、按模型 / MCP / Skill 的调用统计**——所有 Agent 一个仪表盘。

技术栈：**Tauri 2 + React + TypeScript**（前端）/ **Rust**（数据层）。

![Tokenscope 面板（深色 / 浅色）](docs/screenshot.png)

## macOS 快速安装

Homebrew 是推荐的安装和升级方式：

```bash
brew install --cask sunchj/tokenscope/tokenscope
```

## 它做什么

- 菜单栏图标旁显示当日 Token 数（所有 Agent 合计，如 `⬡ 14.00M`）
- 点击打开面板：Day / Week / Month 切换，以及按起止日期（含首尾日）的自定义区间筛选
- **多 Agent**：检测到 ≥2 个 Agent 时出现过滤 chips（All / Claude / Codex）——All 视图**按 Agent** 堆叠用量，切到单 Agent 时整个面板换成该 Agent 的品牌色（Claude 珊瑚橙 / Codex 青绿）；只装一个 Agent 则界面与经典版完全一致
- 指标：总 Token（input/output）、估算花费、Requests / Sessions
- 项目结算：按本地 Git 仓库 / 工作目录归集用量，可将当前时间段导出为 CSV，不暴露原始路径或对话正文
- 可靠性遥测：统计完成/中止 Turn、工具错误/拒绝，以及中止任务消耗的 Token 与估算成本；从本版本开始增量采集
- Context 健康度：上下文窗口中位/峰值压力、≥80% 提醒、压缩次数与 Codex reasoning Token 占比
- 周额度趋势：对 Codex 快照下采样，展示当前重置周期的每日消耗速度与预计触顶时间
- 三个切片：**按模型** / **按 MCP 调用** / **按 Skill 调用**
- **Codex 配额卡**：直接从 Codex 自己的日志读取当前周额度的已用百分比、plan 与重置倒计时
- 成本甜甜圈（hover 看单模型）、年度活跃热力图
- **只统计用户自己安装的 MCP / Skill**，过滤所有内置工具与自带 MCP

## 数据来源（零侵入，只读）

| 用途 | 路径 |
|------|------|
| Claude 会话日志（Token / 模型 / 工具调用） | `~/.claude/projects/**/*.jsonl` |
| Codex 会话日志（Token / 模型 / 配额） | `~/.codex/sessions/**/*.jsonl`（支持 `CODEX_HOME`） |
| Claude 用户 MCP 白名单 | `~/.claude.json` → `mcpServers` + `projects[*].mcpServers` |
| Codex 用户 MCP 白名单 | 全局及受信任项目 `config.toml` → `[mcp_servers.*]`（`$CODEX_HOME/config.toml`、项目 `.codex/config.toml`） |
| 用户 Skill 白名单 | Claude：`~/.claude/skills/`；Codex：`$CODEX_HOME/skills/`、`~/.agents/skills/` 与项目 `.agents/skills/` |
| 模型价格 | **主**：[models.dev](https://models.dev/api.json)（裸模型名，匹配 CLI 日志）→ **兜底**：[LiteLLM](https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json) → 内置快照。缓存于 `~/Library/Caches/tokenscope/`，24h 刷新，离线回退 |

### 关键处理
- Claude：按 `message.id` 去重（流式/重试会重复 usage）；同一消息跨多行时合并其工具调用，token 只计一次
- Codex：取 `token_count` 事件的每回合增量（`last_token_usage`），模型归属来自前置的 `turn_context`；Codex 的 `cached_input_tokens` 是其 `input_tokens` 的**子集**，已拆分成与 Claude 一致的独立 cache-read 口径——跨 Agent 的 token 数可直接对比
- token 拆分：`input`(未缓存) / `cache`(creation+read) / `output`；UI 默认把 cache 并入 In 显示，并单列「cached %」
- 价格匹配：精确名 → 归一化名（去厂商前缀 + `.`↔`p`，如 `glm-5.1`⇄`glm-5p1`）；models.dev 优先官方裸名价
- 成本按四类 token 分别计价；模型带 `priced` 标记，**两源都查不到的模型只计 Token、UI 标注「暂无定价」**
- 日志只有裸模型名、无厂商信息 → 第三方模型默认取官方厂商价（估算）
- 工具分类：MCP 调用从直接的 `mcp__<server>__*` 名称、Codex tool-search 映射或 Codex App 自定义调用中归属 server，再按**所属 Agent**配置过滤；Claude 的 Skill 工具/斜杠命令及 Codex 对 `SKILL.md` 的读取，会按所属 Agent 的用户/项目 skills 目录匹配；其余忽略

> 花费为按公开价格的**估算**；订阅用户应理解为「等效消费价值」。

### 四类 Token 与计价公式

每条 assistant 消息的 `usage` 给出四个**互斥**的 token 计数(同一 token 不会被重复统计):

| 阶段 | `usage` 字段 | 含义 | 单价(相对 input) |
|------|-------------|------|------------------|
| **Input**(未缓存) | `input_tokens` | 本轮新发送的提示词 token | 1× |
| **Cache 写入** | `cache_creation_input_tokens` | 写入提示缓存的上下文 | 约 1.25× |
| **Cache 命中**(读) | `cache_read_input_tokens` | 从缓存重放的上下文 | 约 0.1×(便宜很多) |
| **Output** | `output_tokens` | 模型生成的 token | 约 5× |

**Tokens**(按周期对消息求和):

```
total = input + cache_creation + cache_read + output
# UI 展示： In = input + cache_creation + cache_read，  Out = output，  cached % = cache_read / total
```

**Cost**(每个阶段各按价格表里自己的单价计算):

```
cost = input            × price.input
     + cache_creation   × price.cache_creation
     + cache_read       × price.cache_read     # 缓存命中按折扣后的 read 单价计费
     + output           × price.output
```

所以缓存命中**不会**按普通 input 计费,而是用专门(更便宜)的 `cache_read` 单价——这就是重度缓存场景下 token 量很大、花费却不高的原因。UI 只是把 cache 折进「In」做展示,计费始终按上面四个独立单价。

## 安装

### macOS：Homebrew（推荐）

Homebrew 是 macOS 的主要安装方式：一条命令完成安装，自动处理
Gatekeeper 隔离属性，后续升级也更简单可控。

```bash
brew install --cask sunchj/tokenscope/tokenscope
```

安装后会自动清除隔离属性（cask 的 `postflight` 已内置 `xattr -cr`），**首次直接打开即可，不会弹「Apple 无法验证」**。

打开一次后即注册为登录项，之后**每次开机自动在菜单栏运行**。

升级：

```bash
brew update && brew upgrade --cask tokenscope
```

### macOS：下载 .dmg（备用）

1. 从 [Releases](https://github.com/SunChJ/tokenscope-remix/releases) 下载最新的 `Tokenscope_*_universal.dmg`（同时支持 Apple Silicon 与 Intel）
2. 拖入「应用程序」
3. 因为是**未签名 / 未公证**构建，首次打开会被 Gatekeeper 拦截，二选一：
   - 右键 App →「打开」→ 再次确认「打开」，或
   - 终端执行一次：
     ```bash
     xattr -cr /Applications/Tokenscope.app && open /Applications/Tokenscope.app
     ```

> 未签名是当前的已知限制。要彻底「双击直开」需 Apple Developer ID 签名 + 公证，见 `PRD.md` §6.4。

### Windows

1. 从 [Releases](https://github.com/SunChJ/tokenscope-remix/releases) 下载最新的 `Tokenscope_*_x64-setup.exe`
2. 双击安装。因为是**未签名**构建，首次运行会被 SmartScreen 拦截 —— 点 **"更多信息" → "仍要运行"** 即可
3. 安装器按当前用户安装（无需管理员权限），并**自动注册开机自启**
4. 系统要求：**Windows 10 1803 及以上 / Windows 11**，需要 WebView2 运行时（Win 11 预装；Win 10 用户若没装，安装器会引导补装）

### 更新

App 会在启动时及此后每小时检测新版本。当前版本和更新操作常驻于左上角
**Tokenscope** 下方。Homebrew 用户建议继续使用 `brew upgrade --cask tokenscope`；
直接下载安装的用户可在 App 内完成更新和重启。右键托盘菜单中的
**Check for Updates…** 可立即手动检查。

### 首次启动后

- **macOS**：菜单栏出现图标 + 当日 Token 数（如 `⬡ 12.40M`）
- **Windows**：系统托盘出现图标。Windows 任务栏托盘 API 不支持在图标旁显示文字，**鼠标悬停托盘图标**即可看到当日 Token 数（提示气泡形如 `Tokenscope · today 12.40M`）
- 左键点击图标开/关面板，右键出菜单（Open / Refresh / Check for Updates / 显示偏好 / Quit）
- **Weekly Remaining** 可配置菜单栏 Token 数后的周剩余额度：**Off**、仅 **Codex**（如 `12.40M-76%`），或 **Codex + Spark**（`codex_bengalfox`，显示为 `12.40M-C76%-S93%`）；默认关闭，选择会被记住
- **Dashboard Shortcut** 可用全局快捷键切换面板的显示/隐藏（默认 macOS 为 `⌥⌘T`，Windows/Linux 为 `Ctrl+Alt+T`）；通过 **Change Dashboard Shortcut…** 可直接录入自定义组合键。默认关闭，选择会被记住
- 已自动设置**登录自启**，无需手动配置

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

## 构建

```bash
pnpm tauri build       # macOS 产出 .app / .dmg，Windows 产出 .exe (NSIS)，均位于 src-tauri/target/release/bundle/
```

分发见 `PRD.md` §6.3（macOS 推荐 Homebrew Cask；`.dmg` / `.exe` 直接下载建议代码签名 + 公证）。

## 发布

使用仓库内置脚本同步所有版本文件：

```bash
pnpm version:set 1.3.4
pnpm version:check
git commit -am "release: v1.3.4"
git push origin main
```

版本号变更推送到 `main` 后，GitHub Actions 会校验版本、创建 `vX.Y.Z`
标签，并自动构建和发布 macOS / Windows 安装包、签名更新包和 `latest.json`。
发布失败时，也可以在 Release workflow 中选择已有的同版本标签手工重跑。
发布流程需要配置 `TAURI_SIGNING_PRIVATE_KEY` 和
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets。

## 结构

```
src/                  React 前端
  data.ts             类型 + Tauri 桥 + 主题 + 格式化
  charts.tsx          图表原语（柱状/甜甜圈/sparkline/热力图/分段控件）
  App.tsx             主面板
src-tauri/src/
  store.rs            多源 JSONL 增量摄取（Claude + Codex 适配器）
  parser.rs           按 Scope 聚合（All / 单 Agent；预设周期 + 自定义区间 + 热力图）
  pricing.rs          models.dev / LiteLLM 价格加载与计价
  config.rs           用户 MCP / Skill 白名单（Claude + Codex）
  model.rs            返回给前端的数据结构
  lib.rs              Tauri 命令 + 菜单栏托盘
```

## Bug 记录

开发过程中遇到的典型 bug（现象、根因、解决办法）汇总在
[docs/BUGFIXES.md](docs/BUGFIXES.md)。

## 致谢

本项目源自 [@HduSy](https://github.com/HduSy) 的 [tokenscope](https://github.com/HduSy/tokenscope)——一个非常出色的 macOS 菜单栏 Claude CLI 用量仪表盘。本仓库继承的 Rust 数据摄取/聚合架构与面板设计均出自原作者之手，并沿用原项目的 MIT 协议，在此致以诚挚感谢！🙏

Tokenscope Remix 在其基础上扩展为多 Agent 仪表盘（Codex 已支持，更多 Agent 规划中——见 [docs/DESIGN-multi-agent.md](docs/DESIGN-multi-agent.md)），并独立维护：问题与需求请在本仓库提交，不要打扰上游。
