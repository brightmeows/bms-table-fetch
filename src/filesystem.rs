//! Filesystem utilities: filename sanitisation, JSON sorting, change detection, atomic writes.

use std::path::{Path, PathBuf};

use log::warn;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::fs;

/// Replace characters invalid in filenames with full-width equivalents and collapse consecutive underscores.
#[must_use]
pub fn sanitize_filename(name: &str) -> String {
    // 将非法的非控制字符替换为对应的全角字符；控制字符替换为下划线
    let mapped: String = name
        .chars()
        .map(|c| match c {
            '<' => '＜',
            '>' => '＞',
            ':' => '：',
            '"' => '＂',
            '/' => '／',
            '\\' => '＼',
            '|' => '｜',
            '?' => '？',
            '*' => '＊',
            // 控制字符
            c if c as u32 <= 31 => '_',
            _ => c,
        })
        .collect();

    // 折叠连续下划线（可能来自多个控制字符）
    let mut collapsed = String::with_capacity(mapped.len());
    let mut last_was_us = false;
    for ch in mapped.chars() {
        if ch == '_' {
            if !last_was_us {
                collapsed.push(ch);
                last_was_us = true;
            }
        } else {
            collapsed.push(ch);
            last_was_us = false;
        }
    }

    // Windows 禁止以 '.' 或 ' ' 结尾，将结尾的这些字符替换为对应全角
    let s = collapsed;
    let mut run_start = s.len();
    let indices: Vec<(usize, char)> = s.char_indices().collect();
    for (pos, ch) in indices.iter().rev().copied() {
        if ch == '.' || ch == ' ' {
            run_start = pos;
        } else {
            break;
        }
    }
    if run_start < s.len() {
        let prefix = &s[..run_start];
        let suffix = &s[run_start..];
        let mut replaced = String::with_capacity(suffix.len());
        for ch in suffix.chars() {
            match ch {
                '.' => replaced.push('．'),
                ' ' => replaced.push('　'),
                _ => replaced.push(ch),
            }
        }
        format!("{prefix}{replaced}")
    } else {
        s
    }
}

/// 递归排序 `serde_json::Value`：
/// - 遇到数组：先对每个元素递归处理，然后按字符串表示排序；
/// - 遇到对象：先递归处理其值，然后对当前 `Map` 的 key 排序；
/// - 其他类型：不处理。
pub fn deep_sort_json_value(value: &mut Value) {
    match value {
        Value::Array(arr) => {
            // 排序数组元素
            arr.iter_mut().for_each(deep_sort_json_value);
            // 数组元素排序，确保比较稳定
            arr.sort_by_key(std::string::ToString::to_string);
        }
        Value::Object(map) => {
            // 排序对象值
            map.values_mut().for_each(deep_sort_json_value);
            // 对当前Map的key执行排序，确保键顺序稳定
            map.sort_keys();
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// 判断 JSON 文件内容是否与新的内容不同，支持自定义预处理。
///
/// - 先按字节比较新旧内容（快速路径），相同则直接返回 `false`；
/// - 不同则读取旧文件与新内容，分别解析为 `T`；
/// - 在比较前对两者调用传入的 `preprocess` 函数进行修改；
/// - 比较修改后的值是否不同。
///
/// 返回 `Ok(true)` 表示需要更新（文件不存在、读取/解析失败，或预处理后不相等），否则返回 `Ok(false)`。
///
/// # Errors
///
/// Returns an error if `tokio::fs::try_exists` fails unexpectedly.
pub async fn is_changed<T>(
    path: &Path,
    new_content: &str,
    preprocess: impl Fn(&mut T),
) -> anyhow::Result<bool>
where
    T: DeserializeOwned + PartialEq,
{
    if !fs::try_exists(path).await? {
        // 文件不存在，视为需要更新
        return Ok(true);
    }

    // 读取旧文件内容失败，视为需要更新
    let Ok(old_str) = fs::read_to_string(path).await else {
        warn!("旧文件 {} 读取失败，视为需要更新", path.display());
        return Ok(true);
    };

    // 字节比较快速路径：完全相同则无需进一步解析
    if old_str == new_content {
        return Ok(false);
    }

    // 解析为 T
    let old_parsed = serde_json::from_str::<T>(&old_str);
    let new_parsed = serde_json::from_str::<T>(new_content);

    // 任一解析失败，视为需要更新
    let Ok(mut old_val) = old_parsed else {
        warn!("旧文件 {} 解析失败，视为需要更新", path.display());
        return Ok(true);
    };
    let Ok(mut new_val) = new_parsed else {
        warn!("新内容解析失败，视为需要更新");
        return Ok(true);
    };

    // 比较前进行预处理
    preprocess(&mut old_val);
    preprocess(&mut new_val);

    Ok(old_val != new_val)
}

/// 原子写入文件：先写到一个 `path` + `.tmp` 后缀的临时文件，再 `rename` 覆盖目标。
///
/// 在 POSIX 系统上 `rename` 是原子的，可消除并发写入的竞态。
/// 如果写入过程中进程崩溃，`.tmp` 文件不会影响已存在的目标文件。
///
/// # Errors
///
/// 返回写入或重命名时的 I/O 错误。
pub async fn write_atomic(path: &Path, content: &str) -> anyhow::Result<()> {
    // 追加 `.tmp` 而非替换扩展名，避免与原始扩展名为 `tmp` 的文件冲突
    let tmp_path = {
        let mut p = path.as_os_str().to_os_string();
        p.push(".tmp");
        PathBuf::from(p)
    };
    fs::write(&tmp_path, content).await?;
    fs::rename(&tmp_path, path).await?;
    Ok(())
}

/// 清理目录树中残留的 `.tmp` 文件（上次写入崩溃遗留的）。
///
/// 使用迭代（显式栈）避免递归深度限制和 `Box::pin`。
///
/// # Errors
///
/// 返回扫描或删除时的 I/O 错误。
pub async fn clean_tmp_files(root: &Path) -> anyhow::Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            continue;
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(e)) => e,
                Ok(None) => break,
                Err(_) => continue,
            };
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "tmp") {
                fs::remove_file(&path).await.ok();
            } else if entry.file_type().await.is_ok_and(|t| t.is_dir()) {
                stack.push(path);
            }
        }
    }
    Ok(())
}
