//! 缓存 usage 模拟
//!
//! Kiro 上游不支持 prompt caching，但官方 API 客户端在开启 `cache_control`
//! （5m / 1h）后期望响应 `usage` 中带 `cache_creation_input_tokens` /
//! `cache_read_input_tokens` / `cache_creation` 结构。本模块在代理层**模拟**
//! 这些字段：按 admin-ui 可调的「创建比例 / 命中比例 / 可缓存比例」把估算的
//! 输入 token 总数拆分成三部分。默认关闭，关闭时不注入任何缓存字段。

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// 缓存 TTL（对应官方 `cache_control.ttl`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    /// 默认 5 分钟
    FiveMin,
    /// 1 小时
    OneHour,
}

/// 缓存模拟设置（可在 admin-ui 调整并持久化）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CacheSimSettings {
    /// 是否启用模拟（默认 false）
    #[serde(default)]
    pub enabled: bool,
    /// cache_creation 占「可缓存基数」的比例 [0,1]
    #[serde(default = "default_creation_ratio")]
    pub creation_ratio: f64,
    /// cache_read 占「可缓存基数」的比例 [0,1]
    #[serde(default = "default_hit_ratio")]
    pub hit_ratio: f64,
    /// prompt 中算作「可缓存基数」的比例 [0,1]（默认 1.0，即整段输入）
    #[serde(default = "default_cacheable_ratio")]
    pub cacheable_ratio: f64,
}

fn default_creation_ratio() -> f64 {
    0.25
}
fn default_hit_ratio() -> f64 {
    0.70
}
fn default_cacheable_ratio() -> f64 {
    1.0
}

impl Default for CacheSimSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            creation_ratio: default_creation_ratio(),
            hit_ratio: default_hit_ratio(),
            cacheable_ratio: default_cacheable_ratio(),
        }
    }
}

impl CacheSimSettings {
    /// 将三个比例 clamp 到 [0,1]
    pub fn clamped(mut self) -> Self {
        self.creation_ratio = self.creation_ratio.clamp(0.0, 1.0);
        self.hit_ratio = self.hit_ratio.clamp(0.0, 1.0);
        self.cacheable_ratio = self.cacheable_ratio.clamp(0.0, 1.0);
        self
    }
}

/// 模拟出的缓存 usage 拆分结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheUsage {
    pub input_tokens: i32,
    pub cache_creation_input_tokens: i32,
    pub cache_read_input_tokens: i32,
    pub ttl: CacheTtl,
}

impl CacheUsage {
    /// 把缓存字段写入给定的 usage JSON 对象（同时覆写 `input_tokens`）。
    ///
    /// `usage` 必须是 JSON 对象；非对象时静默跳过。
    pub fn apply_to_usage(&self, usage: &mut serde_json::Value) {
        let Some(obj) = usage.as_object_mut() else {
            return;
        };
        obj.insert("input_tokens".to_string(), json!(self.input_tokens));
        obj.insert(
            "cache_creation_input_tokens".to_string(),
            json!(self.cache_creation_input_tokens),
        );
        obj.insert(
            "cache_read_input_tokens".to_string(),
            json!(self.cache_read_input_tokens),
        );
        let (ephemeral_5m, ephemeral_1h) = match self.ttl {
            CacheTtl::FiveMin => (self.cache_creation_input_tokens, 0),
            CacheTtl::OneHour => (0, self.cache_creation_input_tokens),
        };
        obj.insert(
            "cache_creation".to_string(),
            json!({
                "ephemeral_5m_input_tokens": ephemeral_5m,
                "ephemeral_1h_input_tokens": ephemeral_1h,
            }),
        );
    }
}

/// 根据设置与 TTL，对总输入 token 数 `total` 做缓存拆分。
///
/// 关闭或 `total <= 0` 时返回 `None`（调用方此时保持原有 usage 不变）。
///
/// `input_tokens` 恒 `>= 1`：即便 `creation_ratio + hit_ratio` 配置到接近/等于
/// 1.0，本轮请求实际发生的新增输入也不应被缓存吞成 0——0 在 Anthropic 语义下
/// 意味着"这轮没有任何新输入"，会让缓存计费失去意义。因此可分配给
/// creation+read 的上限固定为 `total - 1`，把至少 1 个 token 留给 input_tokens。
pub fn simulate(settings: &CacheSimSettings, ttl: CacheTtl, total: i32) -> Option<CacheUsage> {
    if !settings.enabled || total <= 0 {
        return None;
    }

    // total == 1 时无空间可分给缓存，直接全部计入 input_tokens。
    if total == 1 {
        return Some(CacheUsage {
            input_tokens: 1,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            ttl,
        });
    }

    // 可分配给 creation+read 的上限：留至少 1 个 token 给 input_tokens。
    let cacheable_cap = total - 1;

    let total_f = total as f64;
    let base = (total_f * settings.cacheable_ratio).round().min(cacheable_cap as f64);
    let mut creation = (base * settings.creation_ratio).round();
    let mut read = (base * settings.hit_ratio).round();

    // creation + read 不得超过可缓存基数 base：超出则按比例缩放
    if creation + read > base {
        let sum = creation + read;
        if sum > 0.0 {
            creation = (creation / sum * base).round();
            read = (read / sum * base).round();
        }
    }

    let mut creation = creation as i32;
    let mut read = read as i32;

    // 双重保险：creation + read 不得超过 cacheable_cap（= total - 1）
    if creation > cacheable_cap {
        creation = cacheable_cap;
    }
    if creation + read > cacheable_cap {
        read = (cacheable_cap - creation).max(0);
    }

    let input_tokens = (total - creation - read).max(1);

    Some(CacheUsage {
        input_tokens,
        cache_creation_input_tokens: creation,
        cache_read_input_tokens: read,
        ttl,
    })
}

/// 递归探测请求 JSON 中是否含 `cache_control` 及其 TTL。
///
/// 扫描所有位置（system / messages / tools 等）；任一断点带 `ttl: "1h"`
/// 则整体按 1h，否则按 5m；完全没有 `cache_control` 返回 `None`。
pub fn detect_cache_ttl(raw: &serde_json::Value) -> Option<CacheTtl> {
    fn walk(v: &serde_json::Value, found: &mut bool, one_hour: &mut bool) {
        match v {
            serde_json::Value::Object(map) => {
                if let Some(cc) = map.get("cache_control") {
                    *found = true;
                    if cc.get("ttl").and_then(|t| t.as_str()) == Some("1h") {
                        *one_hour = true;
                    }
                }
                for val in map.values() {
                    walk(val, found, one_hour);
                }
            }
            serde_json::Value::Array(arr) => {
                for val in arr {
                    walk(val, found, one_hour);
                }
            }
            _ => {}
        }
    }

    let mut found = false;
    let mut one_hour = false;
    walk(raw, &mut found, &mut one_hour);

    if !found {
        None
    } else if one_hour {
        Some(CacheTtl::OneHour)
    } else {
        Some(CacheTtl::FiveMin)
    }
}

/// 共享、可持久化的缓存模拟设置存储。
///
/// anthropic 消息处理器（读）与 admin 服务（读写）共享同一个 `Arc`。
/// 持久化到磁盘（config.json 同目录的 `cache_sim.json`）。
pub struct CacheSimStore {
    inner: RwLock<CacheSimSettings>,
    path: Option<PathBuf>,
}

impl CacheSimStore {
    /// 从文件加载（文件不存在或解析失败时使用默认设置：关闭）。
    pub fn load(path: Option<PathBuf>) -> Arc<Self> {
        let settings = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|c| serde_json::from_str::<CacheSimSettings>(&c).ok())
            .map(|s| s.clamped())
            .unwrap_or_default();

        Arc::new(Self {
            inner: RwLock::new(settings),
            path,
        })
    }

    /// 读取当前设置快照
    pub fn snapshot(&self) -> CacheSimSettings {
        self.inner.read().clone()
    }

    /// 更新设置（clamp + 持久化），返回最终生效的设置。
    pub fn update(&self, new: CacheSimSettings) -> CacheSimSettings {
        let v = new.clamped();
        *self.inner.write() = v.clone();
        self.persist(&v);
        v
    }

    fn persist(&self, s: &CacheSimSettings) {
        let Some(path) = &self.path else {
            return;
        };
        match serde_json::to_string_pretty(s) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存 cache_sim 设置失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化 cache_sim 设置失败: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(creation: f64, hit: f64, cacheable: f64) -> CacheSimSettings {
        CacheSimSettings {
            enabled: true,
            creation_ratio: creation,
            hit_ratio: hit,
            cacheable_ratio: cacheable,
        }
    }

    #[test]
    fn test_disabled_returns_none() {
        let s = CacheSimSettings::default(); // enabled = false
        assert!(simulate(&s, CacheTtl::FiveMin, 1000).is_none());
    }

    #[test]
    fn test_zero_total_returns_none() {
        let s = enabled(0.2, 0.7, 1.0);
        assert!(simulate(&s, CacheTtl::FiveMin, 0).is_none());
    }

    #[test]
    fn test_basic_split() {
        let s = enabled(0.2, 0.7, 1.0);
        let u = simulate(&s, CacheTtl::FiveMin, 1000).unwrap();
        // cacheable_cap = total - 1 = 999，保留至少 1 个 token 给 input_tokens
        assert_eq!(u.cache_creation_input_tokens, 200);
        assert_eq!(u.cache_read_input_tokens, 699);
        assert_eq!(u.input_tokens, 101);
        assert!(u.input_tokens >= 1, "input_tokens 必须恒 >= 1");
        // input + creation + read == total
        assert_eq!(
            u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens,
            1000
        );
    }

    #[test]
    fn test_input_tokens_never_zero_even_at_full_ratio() {
        // creation + hit 恰好覆盖全部（旧实现会让 input_tokens 归零，语义上等于
        // "本轮没有任何新输入"，与"缓存计费"的定义矛盾）
        let s = enabled(0.5, 0.5, 1.0);
        for total in [1, 2, 10, 100, 1000, 100_000] {
            let u = simulate(&s, CacheTtl::FiveMin, total).unwrap();
            assert!(
                u.input_tokens >= 1,
                "total={total} 时 input_tokens={} 不应小于 1",
                u.input_tokens
            );
            assert_eq!(
                u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens,
                total
            );
        }
    }

    #[test]
    fn test_total_one_all_input_no_cache() {
        let s = enabled(0.5, 0.5, 1.0);
        let u = simulate(&s, CacheTtl::FiveMin, 1).unwrap();
        assert_eq!(u.input_tokens, 1);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn test_cacheable_ratio_caps_base() {
        // 可缓存基数只有一半
        let s = enabled(0.2, 0.7, 0.5);
        let u = simulate(&s, CacheTtl::FiveMin, 1000).unwrap();
        // base=500 → creation=100, read=350, input=550
        assert_eq!(u.cache_creation_input_tokens, 100);
        assert_eq!(u.cache_read_input_tokens, 350);
        assert_eq!(u.input_tokens, 550);
    }

    #[test]
    fn test_ratios_exceeding_one_are_scaled_to_base() {
        // creation + hit = 1.5 > 1 → 缩放到 base，input 恒 >= 1
        let s = enabled(0.6, 0.9, 1.0);
        let u = simulate(&s, CacheTtl::FiveMin, 1000).unwrap();
        assert!(u.input_tokens >= 1);
        assert!(
            u.cache_creation_input_tokens + u.cache_read_input_tokens <= 999,
            "creation+read 不得超过 total - 1"
        );
    }

    #[test]
    fn test_ttl_buckets() {
        let s = enabled(0.2, 0.7, 1.0);
        let mut usage = json!({"output_tokens": 5});
        simulate(&s, CacheTtl::OneHour, 1000)
            .unwrap()
            .apply_to_usage(&mut usage);
        assert_eq!(usage["cache_creation"]["ephemeral_1h_input_tokens"], 200);
        assert_eq!(usage["cache_creation"]["ephemeral_5m_input_tokens"], 0);
        assert_eq!(usage["input_tokens"], 101);
        assert_eq!(usage["cache_read_input_tokens"], 699);

        let mut usage5 = json!({});
        simulate(&s, CacheTtl::FiveMin, 1000)
            .unwrap()
            .apply_to_usage(&mut usage5);
        assert_eq!(usage5["cache_creation"]["ephemeral_5m_input_tokens"], 200);
        assert_eq!(usage5["cache_creation"]["ephemeral_1h_input_tokens"], 0);
    }

    #[test]
    fn test_detect_cache_ttl_none() {
        let v = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(detect_cache_ttl(&v), None);
    }

    #[test]
    fn test_detect_cache_ttl_in_system_default_5m() {
        let v = json!({
            "system": [{"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}],
            "messages": []
        });
        assert_eq!(detect_cache_ttl(&v), Some(CacheTtl::FiveMin));
    }

    #[test]
    fn test_detect_cache_ttl_1h_wins() {
        let v = json!({
            "system": [{"type": "text", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "t", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]
        });
        assert_eq!(detect_cache_ttl(&v), Some(CacheTtl::OneHour));
    }

    #[test]
    fn test_detect_cache_ttl_in_messages() {
        let v = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "big ctx", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        assert_eq!(detect_cache_ttl(&v), Some(CacheTtl::FiveMin));
    }
}
