# AGENTS.md

从远程源拉取 BMS 难度表索引和表数据，供下游镜像仓库消费。核心数据模型和 fetcher 由 `bms-table` crate 提供。

## 命令

### Pre-commit（提交时自动触发）

```sh
pre-commit run --all-files --quiet    # 手动触发全部 hooks
```

Hooks：`cargo fmt --check`、`cargo clippy --workspace --quiet`、`cargo doc --workspace --no-deps --quiet`、no-comment-decorations、no-confusable-unicode。

### CI / 手动

```sh
cargo build --release
cargo test
cargo deny check               # 许可证见 deny.toml
```

## 默认流水线

不带参数时由 `SyncEngine`（`src/engine.rs`）编排，分 4 阶段执行：

```
list → overlay → fetch → post_process
```

各阶段职责：

1. **list** — 拉取远程列表写入 `lists/*.json`（`fetch::fetch_list_sources`）；单个源失败时保留旧缓存文件
2. **overlay** — 扫描 `tables/`（`scan::scan_dirs`，仅读 `info.json`）获取 `old_dir_map`，从 `lists/*.json` + `config/table.toml` 计算 active 表集合（`overlay::build_active_set`），预重命名目录（`rename::compute_renames` + `execute_renames`）
3. **fetch** — 并发抓取所有 active 表（无并发上限——预期行为），写入 `tables/*/`（`fetch::fetch_and_save_table`）
4. **post_process** — 全量扫描 `tables/`（`scan::scan_dirs_full`，读 `info.json` + `data.json`），顺序执行：安全网 rename → 移 orphan → 并行写 `tables/tables.json` + `indexes/*.json` → 写 `tables/state.toml`

数据流向：

```
config/list.toml                     → lists/*.json
lists/*.json + config/table.toml     → ActiveSet (三层叠加)
tables/*/ (base + old_dir_map)       ────────┘
ActiveSet                            → tables/*/
tables/*/                            → tables/tables.json + indexes/*.json + tables/state.toml
```

### 与子命令的关系

默认流水线通过 `SyncEngine` 一次性完成全部 4 阶段，扫描 `tables/` 两次（overlay 轻量扫描 + post_process 全量扫描）。6 个子命令（`list` / `tables` / `reconcile` / `cleanup` / `tables-list` / `index`）作为调试/恢复工具保留，每个子命令委托到对应模块，自行扫描。

### Overlay 模型（三层叠加）

活跃表集合由 `overlay::build_active_set` 计算，三层合并（后层覆盖前层）：

1. **Base**（磁盘 `info.json`）——上次运行已 fetch 的表作为起点
2. **Lists**（`lists/*.json`）——覆盖/补充 base 条目
3. **Config**（`config/table.toml`）——add/replace/disable 覆盖规则

`extend` 只添加/覆盖，不移除——不在列表中但磁盘上存在的表会被保留。唯一移除途径：`config/table.toml` 的 `[[disable]]` 规则。

列表源下线时保留上次成功缓存的 `lists/*.json`，不收缩 active set。

### 目录重命名

所有重命名通过 `rename::expected_dir_name` 统一计算目录期望名（`[domain] sanitize(name)`）。三个调用点：

1. **Overlay 阶段**：`compute_renames(entries, Some(&overlaid_info))` — 用 overlaid info 预重命名，确保 fetch 写入正确目录
2. **Fetch 阶段**（每张表）：`expected_dir_name(&info)` → HTTP → 响应实际 domain/name → 二次重命名
3. **Post_process**：`compute_renames(entries, None)` — 以磁盘 info.json 为真相源做安全网重命名，不使用 overlaid info（避免与 fetch 写入的 info.json 不一致导致振荡）

## 模块结构

```
src/
  engine.rs      — SyncEngine 编排器（仅编排，不含业务逻辑）
  overlay.rs     — ActiveSet 计算 + apply_config + load_list_files
  fetch.rs       — fetch_and_save_table + patch_data_url + fetch_list_sources
  rename.rs      — expected_dir_name + compute_renames + execute_renames + maybe_rename_dir
  scan.rs        — scan_dirs（轻量）/ scan_dirs_full（含 data.json）
  orphan.rs      — compute_orphans + execute_orphans
  output.rs      — write_tables_json + write_indexes
  index.rs       — extract_chart_items + maybe_insert + maybe_insert_hash（纯函数）
  state.rs       — SHA3-256 状态跟踪 → tables/state.toml
  config/        — list.rs（ListConfig）/ table.rs（TableConfig）
  filesystem.rs  — sanitize_filename / is_changed / write_atomic / deep_sort_json_value
  logger.rs      — 双通道日志（console + warnings.log）
  cmd/           — 6 个子命令（薄壳，委托到上述模块）
```

依赖方向：`engine` / `cmd/*` → 业务模块（`overlay` / `fetch` / `rename` / `orphan` / `output`）→ 基础设施（`scan` / `config` / `filesystem` / `index`）。底层不依赖上层，`cmd/*` 不被业务模块导入。

## 约束

- **全异步 IO**——`clippy.toml` 禁用 `std::fs::*` 和 `std::thread::spawn`，必须用 `tokio::fs` / `tokio::spawn`
- **条件写入**——写文件前用 `filesystem::is_changed` 检测，跳过无变化的写入（带字节比较快速路径）
- **原子写入**——`filesystem::write_atomic` 先写 `.tmp` 再 `rename` 覆盖
- **`tables/` 扫描仅 2 次**——overlay（`scan_dirs`，仅 `info.json`）和 post_process（`scan_dirs_full`，`info.json` + `data.json`），不自发额外扫描
- **并发抓取无限制**——`fetch` 阶段所有表同时发起 HTTP 请求（通过 `JoinSet`），不设并发上限
- **目录名**——须经 `sanitize_filename` 处理，确保跨平台合法
- **`_orphaned` 是保留名**——所有命令扫描时跳过此目录
- **`tables/state.toml`**——`post_process` 末尾写入，因时间戳始终更新，不使用 `is_changed` 跳过
- **`publish = false`**——不发布到 crates.io

## Git 工作流

Conventional Commits：`feat:` / `fix:` / `refactor:` / `ci:` 等。

## 生成目录

`lists/`、`tables/`、`indexes/` 为 gitignore 的生成目录，由子命令写入，不手动编辑。
