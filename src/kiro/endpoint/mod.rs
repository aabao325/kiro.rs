//! Kiro 端点抽象
//!
//! 不同 Kiro 端点（如 `ide` / `cli`）在 URL、请求头、请求体上存在差异，
//! 但共享凭据池、Token 刷新、重试逻辑和 AWS event-stream 响应解码。
//!
//! [`KiroEndpoint`] 抽象了请求侧的差异点；`KiroProvider` 持有一个 endpoint 注册表，
//! 按凭据的 `endpoint` 字段选择对应实现。

use reqwest::RequestBuilder;

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

pub mod ide;

pub use ide::IdeEndpoint;

/// Kiro 端点
///
/// 同一个 `KiroProvider` 可持有多个 endpoint 实现，按凭据级字段切换。
pub trait KiroEndpoint: Send + Sync {
    /// 端点名称（对应 credentials.endpoint / config.defaultEndpoint 的取值）
    fn name(&self) -> &'static str;

    /// API endpoint URL
    fn api_url(&self, ctx: &RequestContext<'_>) -> String;

    /// MCP endpoint URL
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String;

    /// 装饰 API 请求的端点特有 header
    ///
    /// Provider 已经设置好 URL、content-type、Connection 和 body；
    /// 实现负责追加 Authorization、host、user-agent 等端点相关头。
    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 装饰 MCP 请求的端点特有 header
    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder;

    /// 对已序列化的 API 请求体做端点特有加工（如注入 profileArn）
    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String;

    /// 对已序列化的 MCP 请求体做端点特有加工（默认不变）
    fn transform_mcp_body(&self, body: &str, _ctx: &RequestContext<'_>) -> String {
        body.to_string()
    }

    /// 判断响应体是否表示"上游 bearer token 失效"（触发强制刷新）
    fn is_bearer_token_invalid(&self, body: &str) -> bool {
        default_is_bearer_token_invalid(body)
    }
}

/// 装饰请求时可用的上下文
///
/// 包含单次调用已确定的所有运行时信息。引用形式避免无谓 clone。
pub struct RequestContext<'a> {
    /// 当前凭据
    pub credentials: &'a KiroCredentials,
    /// 有效的 access token（API Key 凭据下即 kiroApiKey）
    pub token: &'a str,
    /// 当前凭据对应的 machineId
    pub machine_id: &'a str,
    /// 全局配置
    pub config: &'a Config,
}

/// 判断响应体是否表示"额度用尽"（禁用凭据并转移）
///
/// 采用配置驱动的关键词子串匹配（大小写敏感），而非结构化解析固定字段，
/// 以应对官方随时可能更改的错误 reason 值。关键词列表可在 Admin UI 中增删，
/// 详见 [`Config::quota_exceeded_keywords`](crate::model::config::Config::quota_exceeded_keywords)。
pub fn is_quota_exceeded(body: &str, keywords: &[String]) -> bool {
    keywords
        .iter()
        .any(|k| !k.is_empty() && body.contains(k.as_str()))
}

/// 默认的 bearer token 失效判断逻辑
pub fn default_is_bearer_token_invalid(body: &str) -> bool {
    body.contains("The bearer token included in the request is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_quota_exceeded_detects_default_keyword() {
        let body = r#"{"message":"You have reached the limit.","reason":"MONTHLY_REQUEST_COUNT"}"#;
        let keywords = vec!["MONTHLY_REQUEST_COUNT".to_string()];
        assert!(is_quota_exceeded(body, &keywords));
    }

    #[test]
    fn test_is_quota_exceeded_detects_custom_keyword() {
        let body = r#"{"message":"You have reached the limit for overages.","reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#;
        let keywords = vec![
            "MONTHLY_REQUEST_COUNT".to_string(),
            "OVERAGE_REQUEST_LIMIT_EXCEEDED".to_string(),
        ];
        assert!(is_quota_exceeded(body, &keywords));
    }

    #[test]
    fn test_is_quota_exceeded_false_when_no_match() {
        let body = r#"{"message":"nope","reason":"DAILY_REQUEST_COUNT"}"#;
        let keywords = vec!["MONTHLY_REQUEST_COUNT".to_string()];
        assert!(!is_quota_exceeded(body, &keywords));
    }

    #[test]
    fn test_is_quota_exceeded_empty_keywords_never_matches() {
        let body = r#"{"reason":"MONTHLY_REQUEST_COUNT"}"#;
        assert!(!is_quota_exceeded(body, &[]));
    }

    #[test]
    fn test_default_bearer_token_invalid() {
        assert!(default_is_bearer_token_invalid(
            "The bearer token included in the request is invalid"
        ));
        assert!(!default_is_bearer_token_invalid("unrelated error"));
    }
}
