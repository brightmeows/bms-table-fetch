use std::path::Path;

use log::warn;
use serde_json::Value;

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
        format!("{}{}", prefix, replaced)
    } else {
        s
    }
}

/// 递归排序 serde_json::Value：
/// - 遇到数组：先对每个元素递归处理并调用 sort_all_objects，然后按字符串表示排序；
/// - 遇到对象：先递归处理其值并调用 sort_all_objects，最后对当前对象执行 sort_all_objects；
/// - 其他类型：不处理。
pub fn deep_sort_json_value(value: &mut Value) {
    match value {
        Value::Array(arr) => {
            // 排序数组元素
            arr.iter_mut().for_each(deep_sort_json_value);
            // 数组元素排序，确保比较稳定
            arr.sort_by_key(|a| a.to_string());
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

/// 判断 JSON 文件内容是否与新的内容不同。
/// 返回 `Ok(true)` 表示需要更新（文件不存在、读取/解析失败，或排序后不相等），否则返回 `Ok(false)`。
use serde::de::DeserializeOwned;

/// 判断 JSON 文件内容是否与新的内容不同，支持自定义预处理。
///
/// - 读取旧文件与新内容，分别解析为 `T`；
/// - 在比较前对两者调用传入的 `preprocess` 函数进行修改；
/// - 比较修改后的值是否不同。
///
/// 返回 `Ok(true)` 表示需要更新（文件不存在、读取/解析失败，或预处理后不相等），否则返回 `Ok(false)`。
pub async fn is_changed<T>(
    path: &Path,
    new_content: &str,
    preprocess: impl Fn(&mut T),
) -> anyhow::Result<bool>
where
    T: DeserializeOwned + PartialEq,
{
    use tokio::fs;

    if !fs::try_exists(path).await? {
        // 文件不存在，视为需要更新
        return Ok(true);
    }

    // 读取旧文件内容失败，视为需要更新
    let Ok(old_str) = fs::read_to_string(path).await else {
        warn!("旧文件 {:?} 读取失败，视为需要更新", path);
        return Ok(true);
    };

    // 解析为 T
    let old_parsed = serde_json::from_str::<T>(&old_str);
    let new_parsed = serde_json::from_str::<T>(new_content);

    // 任一解析失败，视为需要更新
    let Ok(mut old_val) = old_parsed else {
        warn!("旧文件 {:?} 解析失败，视为需要更新", path);
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
