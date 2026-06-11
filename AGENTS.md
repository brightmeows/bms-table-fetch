# AGENTS.md

从远程源拉取 BMS 难度表索引和表数据，供下游镜像仓库消费。核心数据模型和 fetcher 由 `bms-table` crate 提供。

## 命令

```sh
cargo build --release
cargo test
cargo clippy -- -D warnings    # lint 规则见 Cargo.toml [lints] 和 clippy.toml
cargo fmt
cargo deny check               # 许可证见 deny.toml
```

## 默认流水线

不带参数时由 `SyncEngine`（`src/sync.rs`）编排，分 4 阶段执行：

```
list → overlay → fetch → post_process
```

各阶段职责：

1. **list** — 拉取远程列表写入 `lists/*.json`（同 `list` 子命令）
2. **overlay** — 扫描 `tables/` 读取 `info.json`，检查目录名并立即重命名，再计算三层叠加（base + lists + config），确定 active 表集合
3. **fetch** — 并发抓取所有 active 表（无并发上限——预期行为），写入 `tables/*/`
4. **post_process** — 第二次扫描 `tables/`，顺序执行：rename 目录（安全网）→ 移 orphan → 并行写 `tables/tables.json` + `indexes/*.json` → 写 `tables/state.toml`（审计记录）

数据流向：

```
config/list.toml             → lists/*.json
config/table.toml
  + lists/*.json
  + tables/*/info.json (base) → active_set → tables/*/
tables/*/                     → tables/tables.json
tables/*/                     → indexes/*.json
tables/*/                     → tables/state.toml
```

### 与子命令的关系

默认流水线通过 `SyncEngine` 一次性完成全部 4 阶段，期间只扫描 `tables/` 两次（overlay 阶段 + post_process 阶段）。6 个子命令（`list` / `tables` / `reconcile` / `cleanup` / `tables-list` / `index`）仍然可以作为独立入口使用，每个子命令自行扫描 `tables/`。

### 三层 overlay 模型

表集合按三层合并（后层覆盖前层），由 `sync::build_active_set` 实现：

1. **Base** — 磁盘已有表（读 `tables/*/info.json`）
2. **Overlay** — 列表文件（`lists/*.json`）
3. **Override** — `config/table.toml` 规则（add/replace/disable）

`extend` 只添加/覆盖，不移除——不在列表中但磁盘上存在的表会被保留。唯一移除途径：`config/table.toml` 的 `[[disable]]` 规则。

## 约束

- **全异步 IO**——`clippy.toml` 禁用 `std::fs::*` 和 `std::thread::spawn`，必须用 `tokio::fs` / `tokio::spawn`
- **条件写入**——写文件前用 `filesystem::is_changed` 检测，跳过无变化的写入（带字节比较快速路径，内容相同不解析 JSON）
- **原子写入**——`filesystem::write_atomic` 先写 `.tmp` 再 `rename` 覆盖，崩溃不损坏目标文件
- **`tables/` 扫描仅 2 次**——默认流水线中只在 overlay（扫描目录并重命名，读 `info.json` + 可选 `data.json`）和 post_process（读 `info.json` + `data.json`，另读 `header.json` 用于 state.toml）时各扫描一次，不自发额外扫描
- **并发抓取无限制**——`fetch` 阶段所有表同时发起 HTTP 请求（通过 `JoinSet`），不设并发上限。这是预期行为/项目决策
- **reconcile 默认流水线中不做**——rename 在 `post_process` 中内联处理（`sync::compute_renames` + `execute_renames`），同时 overlay 阶段也做一次预重命名确保 fetch 写入正确命名的目录。`reconcile` 子命令仅手动调用时使用
- **目录名**——须经 `sanitize_filename` 处理，确保跨平台合法；rename 后 `tables.json` 中的 `dir_name` 自动更新
- **`_orphaned` 是保留名**——所有命令扫描时跳过此目录，不可用作表名
- **`tables/state.toml`**——`post_process` 末尾写入，记录全局同步时间、每个 active 表的 SHA3-256 哈希（`info.json` / `header.json` / `data.json`）和检查/变化时间戳。因时间戳始终更新，不使用 `is_changed` 跳过，直接原子写入
- **`publish = false`**——不发布到 crates.io（见 `Cargo.toml`）

## Git 工作流

Conventional Commits：`feat:` / `fix:` / `refactor:` / `ci:` 等。

## 生成目录

`lists/`、`tables/`、`indexes/` 为 gitignore 的生成目录，由子命令写入，不手动编辑。
