# Tokenscope

[English](README.md) · **中文**

**macOS 菜单栏工具**，为本机 **AI 编码 Agent**（Pi、Claude Code、Codex CLI）提供一个统一仪表盘：**Token 用量、估算花费、按模型 / MCP / Skill 的统计，以及实时订阅额度**。

技术栈：**Tauri 2 + React + TypeScript**（前端）+ **Rust**（数据层）。

![Tokenscope 面板（深色 / 浅色）](docs/screenshot.png)

## 亮点

- **一个仪表盘，覆盖所有 Agent** — 过滤 chips（All / Claude / Codex / Pi）在汇总与单 Agent 视图间切换，各自带品牌色主题
- **Token 与成本分析** — 今天 / 本周 / 本月 + 自定义日期区间；选择模型后可查看本地实际出现的推理档位构成，并提供 MCP 调用与 Skill 切片（只统计你自己安装的，内置工具全部过滤）
- **实时订阅额度** — Claude（5 小时 + 每周）与 Codex（每周）每 60 秒或手动刷新时更新，Codex status 风格展示（`29% left (resets 05:01 on 9 Aug)`），含消耗速率预测
- **菜单栏一瞥即知** — 当日 Token 数 + 各 provider 最紧张窗口的紧凑额度摘要（`52.8M · Cl79% · Cx29%`）或完整列表
- **项目结算** — 按 Git 仓库 / 工作目录归集用量，导出 CSV，不暴露原始路径或对话正文
- **可靠性与上下文遥测** — 中止任务浪费、工具错误、上下文窗口压力、压缩次数与 reasoning 占比
- **本地优先、只读** — 就地解析 session 日志；额度来自内置 HappyUsage CLI（不缓存凭据，不用本地日志计算额度）

## 安装

```bash
brew install --cask sunchj/tokenscope/tokenscope
```

DMG 直接下载与更新方式见 [Releases](https://github.com/SunChJ/tokenscope-remix/releases)。

## 快速上手

1. 启动后，菜单栏 / 托盘出现图标和当日 Token 数
2. **左键**切换仪表盘；**右键**打开菜单（额度详情、菜单栏显示、Dashboard 快捷键、语言等）
3. **菜单栏显示**控制 Token 数旁的额度摘要：关闭 / 紧凑 / 详细

## 开发

```bash
pnpm install
pnpm tauri dev         # 桌面 App（需要 Rust 工具链）
pnpm tauri build       # macOS：.app / .dmg
```

## 文档

- 数据来源与统计口径：[docs/DESIGN-usage-api.md](docs/DESIGN-usage-api.md)、[docs/DESIGN-pi-adapter.md](docs/DESIGN-pi-adapter.md)、[docs/DESIGN-multi-agent.md](docs/DESIGN-multi-agent.md)
- 已知问题与修复记录：[docs/BUGFIXES.md](docs/BUGFIXES.md)

## 致谢

本项目源自 [@HduSy](https://github.com/HduSy) 的 [tokenscope](https://github.com/HduSy/tokenscope)——继承的 Rust 数据摄取/聚合架构与面板设计均出自原作者之手，本仓库沿用 MIT 协议。Provider 额度使用 [HappyUsage](https://github.com/SunChJ/happyusage)（`hu`）。问题与需求请在本仓库提交。
