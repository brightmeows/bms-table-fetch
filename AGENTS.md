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

不带参数运行时按以下顺序执行六步：

```
list → tables → reconcile → cleanup → tables_list → index
```

数据流向：

```
config/list.toml             → lists/*.json
config/table.toml
  + lists/*.json
  + tables/*/info.json (base) → tables/*/
tables/*/info.json           → tables/tables.json
tables/*/data.json           → indexes/*.json
```

### 三层 overlay 模型

`tables` 子命令的表集合按三层合并（后层覆盖前层）：

1. **Base** — 磁盘已有表（读 `tables/*/info.json`）
2. **Overlay** — 列表文件（`lists/*.json`）
3. **Override** — `config/table.toml` 规则（add/replace/disable）

`extend` 只添加/覆盖，不移除——不在列表中但磁盘上存在的表会被保留。唯一移除途径：`config/table.toml` 的 `[[disable]]` 规则。

## 约束

- **全异步 IO**——`clippy.toml` 禁用 `std::fs::*` 和 `std::thread::spawn`，必须用 `tokio::fs` / `tokio::spawn`
- **条件写入**——写文件前用 `filesystem::is_changed` 检测，跳过无变化的写入
- **目录名**——须经 `sanitize_filename` 处理，确保跨平台合法
- **`_orphaned` 是保留名**——所有命令扫描时跳过此目录，不可用作表名
- **`publish = false`**——不发布到 crates.io（见 `release-plz.toml`）

## Git 工作流

Conventional Commits：`feat:` / `fix:` / `refactor:` / `ci:` 等（分组定义见 `release-plz.toml` commit_parsers）。release-plz 自动发版。

## 生成目录

`lists/`、`tables/`、`indexes/` 为 gitignore 的生成目录，由子命令写入，不手动编辑。
