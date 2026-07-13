# TokenScope 代码评审报告

> **评审日期**:2026-06-29 ｜ **分支**:`feat/windows-support` ｜ **范围**:整仓(Tauri 2 / Rust 后端 + React 18 / TypeScript 前端)
>
> **方法**:多智能体对抗式审查。12 个 finder 按「文件 × 维度」切片并行读码并对照 PRD;每条发现交由 3 个不同视角的怀疑者(代码核对 / 真实影响 / 框架语境)独立验证,**≥2 票确认为真**才保留。45 条原始发现 → 确认 38 条 → 综合去重合并约 9 条重复后 **29 项**。
>
> 本报告仅收录经 ≥2/3 独立怀疑者对抗式验证为真的发现;7 条低置信/被拒条目(如仅凭 `(size,mtime)` 的变更检测、free-price 模型被报 unknown、CI 缺 `--frozen-lockfile` 等)已剔除。

## 执行摘要

整体健康度中等偏下:核心数据流(解析/聚合/定价/前端展示)逻辑大体正确,但**缓存持久化层的崩溃一致性与重读幂等性是最大隐患**,且 HEAD 提交曾引入一个会让所有平台构建失败的致命配置错误(已修复)。按严重度统计:**Critical 1、High 1、Medium 7、Low 14、Nit 6**(共 29 项)。三大主题:

1. `events.json`/`offsets.json` 缓存的原子性、损坏处理与重读去重缺陷会静默丢失或污染历史数据;
2. 一切都经 `build_dashboard` 每 30s + 每次开 popover 触发,造成 O(全部历史) 的重复全量读写/解析(含主线程阻塞 IO 与锁内网络抓取);
3. 发布流水线脆弱。

**跨平台/Windows 风险尤其突出**:当前 `feat/windows-support` 分支的 Critical 构建中断同时阻断 macOS `.dmg` 与 Windows NSIS 安装器;autostart 在 Windows 上写 `HKCU\...\Run` 且无法关闭;非原子写入与 AV/文件锁在 Windows 上更易触发缓存损坏。

---

## Critical

### 1. 无效的 `"updater"` bundle 目标导致所有平台构建在解析期失败 — ✅ 已修复

- **状态**:已修复(`tauri.conf.json:34` 的 `bundle.targets` 由 `["app","dmg","nsis","updater"]` 改回 `["app","dmg","nsis"]`)。
- **位置**:`src-tauri/tauri.conf.json:34`(`bundle.targets`)
- **说明**:HEAD 提交 `22d2d79` 向 `bundle.targets` 加入 `"updater"`,但在锁定的 Tauri 2(`tauri 2.11.2`/`tauri-utils 2.9.2`)中合法 `BundleType` 只有 `deb/rpm/appimage/msi/nsis/app/dmg`,其手写 `Deserialize` 对未知值直接返回 `Err`。该配置由 `tauri-build` 在编译期反序列化并经 `generate_context!` 嵌入(官方 `config.schema.json` 亦无名为 `updater` 的 bundle target,佐证此判定)。
- **影响**:macOS universal `.dmg` 与 Windows x64 NSIS 两条 release 矩阵均在配置解析阶段中止,下次打 tag 发布产出 0 个产物;本地 `cargo build`/`tauri dev` 同样编译不过。提交本意是「no-op」,实为硬性构建中断(Tauri 2 的 `"all"` 从不含 updater,提交前提是 Tauri 1 心智模型)。
- **修复**:targets 恢复为 `["app","dmg","nsis"]`(已完成)。若确需自更新产物,应用 `bundle.createUpdaterArtifacts: true`(而非 target),并补齐 `tauri-plugin-updater` 依赖、`plugins.updater`(endpoints/pubkey) 与 CI 的 `TAURI_SIGNING_PRIVATE_KEY`(见 Low #5)。

---

## High

### 1. `events.json` 损坏/写一半时静默丢弃 manifest,永久丢失全部历史

- **位置**:`src-tauri/src/store.rs` `load()` 78-89
- **说明**:`events.json` 与 `offsets.json` 在两个独立 `if let Ok` 块中分别反序列化、各自回退默认值。若 `events.json` 缺失/截断/损坏但 `offsets.json` 解析成功,`events` 退空而 `manifest` 仍保留非零逐文件偏移;`ingest()` 随后因 `(size,mtime)` 未变而 `continue` 跳过每个已记录文件,历史事件永不重读。
- **影响**:一次中断的保存(托盘应用被强退/OS kill/断电/磁盘满,Windows 上 AV/文件锁瞬时读失败亦可)只损坏 `events.json`,即可抹掉整个历史聚合(周/月总量、模型分布、MCP/Skill 计数),且无版本号变更、无 manifest 重置,下次 `save()` 还会把截断状态固化,永不恢复。
- **修复**:将两文件视为一致单元:`events.json` 读或解析失败时也把 `manifest` 重置为 default 触发全量重扫;任一反序列化失败就同时丢弃两者从头重扫。

---

## Medium

### 1. 重读已见字节非幂等:MCP/Skill 计数膨胀,无 id 事件 token 重复计

- **位置**:`src-tauri/src/store.rs` `ingest()` —— 截断分支 144-148、merge 分支 174-181、无条件 push 184
- **说明**:检测到文件缩短时偏移重置为 0 全量重读(注释 `dedup protects us` 自 STORE_VERSION v3 起已过时)。对已入库行,dedup 走 merge 分支 `prev.mcp.extend(ev.mcp); prev.skills.extend(ev.skills);`——token 不重复计,但工具调用被无条件再次追加。此外无 `message.id` 的 assistant 事件(`parse_assistant` 用 `unwrap_or("")`)绕过 dedup 被无条件 push,重读时 token 重复计。
- **影响**:会话日志被截断/重写/compact,或缓存写入间崩溃后,per-server MCP 调用数与 per-skill 调用数翻倍,污染 PRD 2.2/2.3 的核心分布;token/cost 总量仍正确使差异难察觉。
- **修复**:让重读对工具调用幂等。检测截断(`size < poff`)时做全局重置,或给 `RawEvent` 加 `source` 字段以便重读前先清除该文件事件;无可用 dedup key 的 assistant 事件应跳过 push 或合成稳定 key。

### 2. `save()` 不具备崩溃一致性:非原子 `fs::write` + 错误被吞

- **位置**:`src-tauri/src/store.rs` `save()` 104-114
- **说明**:依次用 `fs::write` 直接写 `events.json`→`offsets.json`→`version`,无 temp+rename、无 fsync、三个返回值全部丢弃。`fs::write` 先截断后写,中断会留下半截无效 JSON;崩溃可让 `events`/`offsets` 失配。
- **影响**:对频繁被强退/kill 的托盘应用,撕裂写会损坏缓存(喂给 High #1 的历史丢失与本节 #1 的计数膨胀);错误被吞,缓存可能静默停止持久化。每 30s 一次保存让窗口反复出现。
- **修复**:原子写——先写同目录临时文件、`fsync`、再 `fs::rename` 覆盖(同卷在 Windows/Unix 均原子);或把两份数据合到单文件;至少把写错误暴露出来。

### 3. Autostart 每次启动强制开启,应用内无法关闭

- **位置**:`src-tauri/src/lib.rs:583-584`;`src-tauri/capabilities/default.json:17-19`
- **说明**:`setup` 闭包每次启动都无条件 `let _ = app.autolaunch().enable();`。它幂等——但正因如此每次启动都重新登记登录项(Windows `HKCU\...\Run`/macOS LaunchAgent)。前端无任何开关,从不调用 `disable()`/`is_enabled()`,capabilities 授予的 `allow-disable`/`allow-is-enabled` 成了死权限。
- **影响**:用户在系统设置里移除开机项后,下次启动被静默重新添加,应用内无法关闭。违反 PRD 6.3「设置中可开关」与 §7「开机自启可选」。
- **修复**:仅依据持久化用户偏好启用(首启可默认开);暴露调用 `enable()`/`disable()` 并持久化选择的命令,启动时用 `is_enabled()` 与偏好对账。

### 4. 错误页粘滞:瞬时初始加载失败后仪表盘永不恢复

- **位置**:`src/App.tsx` —— `useEffect` 410-431、渲染门 439-450
- **说明**:初次加载失败 `setErr`,渲染门先判 `err` 再判 `dash`,但任何恢复路径都不清 `err`(`dashboard-updated` 监听与 focus 重取只 `setDash`)。后台 30s 线程独立推送 `dashboard-updated`,于是瞬时初始失败后即便有效数据已填入 `dash`,UI 仍永久停在「Failed to load…」。
- **影响**:单次瞬时启动错误把仪表盘锁死整个会话,唯一出路是重启应用。
- **修复**:每次成功取数都清错误(`setDash(d); setErr(null);`),用于初次加载、监听与 focus 重取;或 `dash` 已有时仅显示非阻塞错误条。

### 5. `get_dashboard` 在 UI 线程同步执行重 IO 并争用 `BUILD_LOCK`

- **位置**:`src-tauri/src/lib.rs` 476-491;`parser.rs` `build_dashboard` 70-77
- **说明**:`get_dashboard` 是同步 `#[tauri::command]`,Tauri 在主线程执行。其内部 `build_dashboard` 持 `BUILD_LOCK` 并做阻塞 IO(load 反序列化整个 `events.json`、ingest 扫描、save 重写整缓存)。前端每次开 popover 都调用它且需先抢 `BUILD_LOCK`,而 30s 后台线程可能正持锁扫描。
- **影响**:重度日志时开 popover 会卡住原生事件循环,卡顿随历史规模增长。
- **修复**:命令声明为 `#[tauri::command(async)]` 并用 `spawn_blocking` 跑重活;托盘已由后台线程更新,命令不应阻塞事件循环。

### 6. 事件缓存无限增长,且每 30s 全量重载 + 重写

- **位置**:`src-tauri/src/store.rs` load/ingest/save;`parser.rs:75-77`
- **说明**:每次 `build_dashboard` 都 `load()` 再 `save()`,`events` 只增不裁,而报表只需当月与热力图窗口(~26 周)。每次刷新反序列化整向量、重写整个 `events.json`——即使 `ingest()` 返回 0 新事件(返回值被丢弃)。
- **影响**:重度长期用户 `events.json` 增至数十 MB,per-refresh 成本 O(全部历史),每 30s 永久空转重写,违背 PRD §7「CPU 空闲 < 1%」。
- **修复**:保存前裁剪早于热力图窗口的事件并重建 `index`;`ingest()` 返回 0 时跳过 `save()` 与事件发射;把已加载 store 放入 app state 避免每次重载。

### 7. 定价表每 30s 重新解析,且 24h 边界在 `BUILD_LOCK` 内做阻塞网络抓取

- **位置**:`src-tauri/src/pricing.rs` `Pricing::load()` 83-99;`parser.rs:81`(持 `BUILD_LOCK`)
- **说明**:`build_dashboard` 每次都 `Pricing::load()`,重新读盘并解析 models.dev 与 1MB+ 的 LiteLLM 表、重建随即丢弃的 HashMap。更糟:缓存跨 24h 后,第一次 `load()` 在锁内做两次阻塞 `ureq::get(...).timeout(10s)`(最多 ~20s);过期后网络不可用时每次 30s 刷新都重试该阻塞抓取。
- **影响**:约 2880 次/天整表解析的持续 CPU;每日一次(离线时反复)~20s 锁停顿恰在用户开面板时阻塞仪表盘/托盘。
- **修复**:用 `OnceCell<Mutex<Arc<Pricing>>>` 记忆化只加载一次;把 24h 网络刷新放到独立后台线程,成功后换入新 `Pricing`,使解析与网络都不在 `BUILD_LOCK` 内。

---

## Low

1. **内置「快照」仅 5 个硬编码 Anthropic 模型,非 PRD 的离线全表**(`pricing.rs:159-176`)——首启即离线时 GLM/DeepSeek 等第三方与较老 Claude id 解析为 None,token 计而 cost 静默归零;首次联网后自愈。用 `include_str!` 内置真实快照。
2. **仅 30s 轮询、无 fs 监视器,未达 PRD 5s 刷新**(`lib.rs:731-734`)——无 `notify` 依赖;popover 开着时新用量最长 ~30s 才现。加防抖 fs 监视器。
3. **WebView 从 Google CDN 拉字体,违背本地优先隐私定位**(`index.html:7-9`、`tokenscope-panel.html:7-9`)——每次启动向第三方泄露 IP/UA/时序。woff2 自托管 + 本地 `@font-face`。
4. **CSP 被禁用(`csp: null`)**(`tauri.conf.json:29`)——纵深防御缺口(当前无 `dangerouslySetInnerHTML` 故无现实可利用路径)。设受限 CSP。
5. **自更新半成品:无插件、无 endpoints/pubkey、CI 无签名密钥**——即便修了 Critical #1 也无法自更新。要么彻底放弃,要么补齐全套。
6. **版本漂移:`package.json` 停 0.1.7,Cargo/tauri 已 0.1.19**——发布脚雷,目前因 tag 与 `tauri.conf.json` 一致而侥幸正常。统一真源。
7. **`~/.claude.json` 每次构建被读盘并完整解析两次**(`config.rs:21/45`)——活跃用户该文件可达数 MB,每 30s 解析两次 + N 个项目目录扫描。解析一次复用。
8. **Skill 白名单扫描所有注册项目目录,偏离 PRD 全局源并虚高「已安装」计数**(`config.rs:70-78`)。
9. **`fetch_cached` 持久化任何以 `{` 开头的 200 响应,可毒化缓存 24h**(`pricing.rs:70-79`)——结构校验通过后再覆盖缓存。
10. **Shell 插件与 `shell:allow-open` 能力被启用但从未使用**(`capabilities/default.json:16`、`lib.rs:550`、`Cargo.toml:19`)——纯负债,扩大攻击面,删除。
11. **`workflow_dispatch` 从非 tag ref 触发会产生畸形 release 并使 Homebrew 步骤失败**(`release.yml`)——加 `if: startsWith(github.ref, 'refs/tags/v')` 守卫。
12. **请求趋势 sparkline 计入了请求指标排除的 slash-command 事件**(`parser.rs:288/350/420`)——给趋势自增加 `if !e.model.is_empty()` 守卫。

---

## Nit

1. **`linePath` 对单点序列返回 NaN、空输入抛异常**(`data.ts:41-56`)——生产不可达,但导出函数应自保护。
2. **CostDonut 把第 6+ 名模型并成不可区分的灰色**(`charts.tsx`)——加发丝描边或派生深浅(图例信息不丢)。
3. **异步 Tauri 监听在 StrictMode 双挂载下泄漏**(`App.tsx:416-430`)——仅 dev 受影响,用 `cancelled` 标志注销。
4. **`fmtTokens` 把非零 <1K token 显示为「0K」**(`data.ts:32`)——小值显示真实 token 数。
5. **`<html lang="zh">` 但整个 UI 是英文**(`index.html:2`)——改 `lang="en"` 或本地化。
6. **`tauri-plugin-single-instance` 钉版方式与同类不一致**(`Cargo.toml:28`)——归一为 `"2"`。

---

## 覆盖度与残余风险

- **Windows 特定运行时行为**未经动态验证(`HKCU\...\Run`、WebView2 冷启动时序、AV/文件锁、路径解析多为静态推断)——强烈建议真机验证;尤其 Critical #1 修复后必须实跑通 NSIS 安装器与 `cargo build`。
- **解析/分类正确性**相对 PRD 的逐条对照覆盖不足,建议用真实 JSONL 样本回归。
- **定价匹配语义**(exact vs normalized、免费/0 价模型、models.dev 第二端点)在被拒条目中存在争议,建议人工核对 PRD 4.x。
- **并发竞态**(Medium #1/#2、Low #12)多由代码推理得出,建议在强退/断电/磁盘满/日志重写等中断场景动态复现。
- **性能 NFR**(每 30s 全量解析+重写、定价整表解析)需实际 profiling 量化是否真的破坏 PRD「<1% 空闲 CPU」。
