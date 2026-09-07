//! 可用模型查询数据模型
//!
//! 上游接口：`GET /ListAvailableModels?origin=AI_EDITOR`。

use serde::Deserialize;

/// ListAvailableModels API 响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableModelsResponse {
    #[serde(default)]
    pub models: Vec<UpstreamModel>,
}

/// 上游返回的单个模型
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamModel {
    pub model_id: String,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub token_limits: Option<TokenLimits>,
}

/// 上游模型 Token 限额
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    #[serde(default)]
    pub max_input_tokens: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_available_models() {
        let response: ListAvailableModelsResponse = serde_json::from_str(
            r#"{"models":[{"modelId":"custom-model-v1","modelName":"Custom Model","tokenLimits":{"maxInputTokens":200000}}]}"#,
        )
        .unwrap();

        assert_eq!(response.models[0].model_id, "custom-model-v1");
        assert_eq!(response.models[0].model_name.as_deref(), Some("Custom Model"));
        assert_eq!(
            response.models[0]
                .token_limits
                .as_ref()
                .and_then(|limits| limits.max_input_tokens),
            Some(200000)
        );
    }

    #[test]
    fn missing_models_defaults_to_empty() {
        let response: ListAvailableModelsResponse = serde_json::from_str("{}").unwrap();
        assert!(response.models.is_empty());
    }
}
