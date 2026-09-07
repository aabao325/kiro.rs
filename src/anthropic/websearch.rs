//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成

use std::convert::Infallible;

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::converter::convert_request;
use super::stream::{SseEvent, generate_message_id};
use super::types::{ErrorResponse, Message, MessagesRequest, SystemMessage};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::provider::KiroProvider;
use crate::model::cache_sim::{self, CacheSimSettings, CacheTtl};

/// 缓存模拟上下文别名（与 handlers.rs 一致）
type CacheSimCtx = Option<(CacheSimSettings, CacheTtl)>;

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

/// 检查请求是否为纯 WebSearch 请求
///
/// 条件：tools 有且只有一个，且 name 为 web_search
pub fn has_web_search_tool(req: &MessagesRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools.len() == 1 && tools.first().is_some_and(|t| t.name == "web_search")
    })
}

/// 从消息中提取搜索查询
///
/// 读取 messages 的第一条消息的第一个内容块
/// 并去除 "Perform a web search for the query: " 前缀
pub fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    // 获取第一条消息
    let first_msg = req.messages.first()?;

    // 提取文本内容
    let text = match &first_msg.content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // 获取第一个内容块
            let first_block = arr.first()?;
            if first_block.get("type")?.as_str()? == "text" {
                first_block.get("text")?.as_str()?.to_string()
            } else {
                return None;
            }
        }
        _ => return None,
    };

    // 去除前缀 "Perform a web search for the query: "
    const PREFIX: &str = "Perform a web search for the query: ";
    let query = if text.starts_with(PREFIX) {
        text[PREFIX.len()..].to_string()
    } else {
        text
    };

    if query.is_empty() { None } else { Some(query) }
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 生成8位小写字母和数字的随机字符串
fn generate_random_id_8() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}_{毫秒时间戳}_{8位随机}
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let random_22 = generate_random_id_22();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random_8 = generate_random_id_8();

    let request_id = format!(
        "web_search_tooluse_{}_{}_{}",
        random_22, timestamp, random_8
    );

    // tool_use_id 采用 Anthropic 官方 server_tool_use 格式：srvtoolu_ + 24 位 base62（以 01 开头）
    let tool_use_id = format!("srvtoolu_01{}", generate_random_id_22());

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
            },
        },
    };

    (tool_use_id, request)
}

/// 解析 MCP 响应中的搜索结果
pub fn parse_search_results(mcp_response: &McpResponse) -> Option<WebSearchResults> {
    let result = mcp_response.result.as_ref()?;
    let content = result.content.first()?;

    if content.content_type != "text" {
        return None;
    }

    serde_json::from_str(&content.text).ok()
}

/// 标准 base64 编码（带 padding）。
///
/// 用于 `encrypted_content` 字段——官方此字段是 Anthropic 侧加密 token，
/// 代理无法复刻，这里退化为对 snippet 做 base64（明文可逆），仅为格式贴合。
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 将毫秒时间戳格式化为相对时间（"X hours/days/weeks ago"），对齐官方 `page_age`。
fn relative_page_age(published_ms: i64) -> Option<String> {
    let published = chrono::DateTime::from_timestamp_millis(published_ms)?;
    let now = chrono::Utc::now();
    let delta = now.signed_duration_since(published);
    let secs = delta.num_seconds();
    if secs < 0 {
        // 未来时间，回退为日期
        return Some(published.format("%B %-d, %Y").to_string());
    }
    let mins = secs / 60;
    let hours = secs / 3600;
    let days = secs / 86_400;
    let weeks = days / 7;
    let s = if secs < 60 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{} minute{} ago", mins, if mins == 1 { "" } else { "s" })
    } else if hours < 24 {
        format!("{} hour{} ago", hours, if hours == 1 { "" } else { "s" })
    } else if days < 7 {
        format!("{} day{} ago", days, if days == 1 { "" } else { "s" })
    } else if weeks < 5 {
        format!("{} week{} ago", weeks, if weeks == 1 { "" } else { "s" })
    } else {
        published.format("%B %-d, %Y").to_string()
    };
    Some(s)
}

/// 从对话中提取最近一条用户文本（用于生成搜索词 / 综合答案的意图来源）。
///
/// 优先取最后一条 role=="user" 的文本；失败时回退到 `extract_search_query`。
fn latest_user_text(req: &MessagesRequest) -> Option<String> {
    for msg in req.messages.iter().rev() {
        if msg.role != "user" {
            continue;
        }
        let text = match &msg.content {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Array(arr) => arr.iter().find_map(|b| {
                if b.get("type")?.as_str()? == "text" {
                    Some(b.get("text")?.as_str()?.to_string())
                } else {
                    None
                }
            }),
            _ => None,
        };
        if let Some(t) = text {
            let t = t.trim();
            if !t.is_empty() {
                // 去除 websearch 前缀（与 extract_search_query 一致）
                const PREFIX: &str = "Perform a web search for the query: ";
                return Some(t.strip_prefix(PREFIX).unwrap_or(t).to_string());
            }
        }
    }
    extract_search_query(req)
}

/// 内部模型调用：合成一个非流式请求发往 Kiro，收集 assistant 文本。
///
/// 复用现有 converter / provider / 事件解码链路（与 handlers.rs 非流式同模式）。
async fn model_complete(
    provider: &KiroProvider,
    model: &str,
    system: &str,
    user: &str,
) -> anyhow::Result<String> {
    let req = MessagesRequest {
        model: model.to_string(),
        max_tokens: 2048,
        messages: vec![Message {
            role: "user".to_string(),
            content: serde_json::Value::String(user.to_string()),
        }],
        stream: false,
        system: Some(vec![SystemMessage {
            text: system.to_string(),
        }]),
        tools: None,
        tool_choice: None,
        thinking: None,
        output_config: None,
        metadata: None,
    };

    let conversion = convert_request(&req).map_err(|e| anyhow::anyhow!("转换请求失败: {}", e))?;
    let kiro_request = KiroRequest {
        conversation_state: conversion.conversation_state,
        profile_arn: None,
    };
    let body = serde_json::to_string(&kiro_request)?;

    let response = provider.call_api(&body).await?;
    let bytes = response.bytes().await?;

    let mut decoder = EventStreamDecoder::new();
    decoder.feed(&bytes).ok();

    let mut text = String::new();
    for result in decoder.decode_iter() {
        if let Ok(frame) = result {
            if let Ok(Event::AssistantResponse(resp)) = Event::from_frame(frame) {
                text.push_str(&resp.content);
            }
        }
    }
    Ok(text)
}

/// 生成搜索词：让模型把用户请求改写为带日期的最优网页搜索 query。
///
/// 失败时回退到用户原话（`latest_user_text`），保证不至于整体失败。
async fn generate_search_query(provider: &KiroProvider, payload: &MessagesRequest) -> String {
    let intent = latest_user_text(payload).unwrap_or_default();
    let today = chrono::Utc::now().format("%Y-%m-%d");
    let system = format!(
        "你是搜索词生成器。今天是 {today}。根据用户请求，输出**一行**最优的网页搜索查询语句；\
         若涉及时效性内容请包含必要日期。只输出查询语句本身，不要解释、不要引号、不要前缀。"
    );

    match model_complete(provider, &payload.model, &system, &intent).await {
        Ok(s) => {
            let q = s.trim().trim_matches('"').lines().next().unwrap_or("").trim();
            if q.is_empty() { intent } else { q.to_string() }
        }
        Err(e) => {
            tracing::warn!("生成搜索词失败，回退用户原话: {}", e);
            intent
        }
    }
}

/// 综合答案：让模型根据搜索结果用中文综合回答用户问题。
///
/// 失败时回退到 `generate_search_summary`。
async fn synthesize_answer(
    provider: &KiroProvider,
    model: &str,
    user_intent: &str,
    results: &Option<WebSearchResults>,
) -> String {
    let mut context = String::new();
    if let Some(results) = results {
        for (i, r) in results.results.iter().enumerate() {
            context.push_str(&format!("[{}] {}\n{}\n", i + 1, r.title, r.url));
            if let Some(snippet) = &r.snippet {
                let truncated = match snippet.char_indices().nth(500) {
                    Some((idx, _)) => &snippet[..idx],
                    None => snippet.as_str(),
                };
                context.push_str(truncated);
                context.push('\n');
            }
            context.push('\n');
        }
    }

    if context.trim().is_empty() {
        return generate_search_summary(user_intent, results);
    }

    let system = "你是联网搜索助手。根据下列搜索结果，用中文综合、客观地回答用户的问题，\
         分点陈述，注明来源标题。不要编造搜索结果之外的信息。";
    let user = format!("用户问题：{user_intent}\n\n搜索结果：\n{context}");

    match model_complete(provider, model, system, &user).await {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => generate_search_summary(user_intent, results),
        Err(e) => {
            tracing::warn!("综合搜索答案失败，回退本地摘要: {}", e);
            generate_search_summary(user_intent, results)
        }
    }
}


/// 生成 WebSearch SSE 响应流
pub fn create_websearch_sse_stream(
    model: String,
    query: String,
    tool_use_id: String,
    search_results: Option<WebSearchResults>,
    answer: String,
    input_tokens: i32,
    cache_sim_ctx: CacheSimCtx,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let events = generate_websearch_events(
        &model,
        &query,
        &tool_use_id,
        search_results,
        &answer,
        input_tokens,
        &cache_sim_ctx,
    );

    stream::iter(
        events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    )
}

/// 生成 WebSearch SSE 事件序列
fn generate_websearch_events(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: Option<WebSearchResults>,
    answer: &str,
    input_tokens: i32,
    cache_sim_ctx: &CacheSimCtx,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = generate_message_id();

    // message_start 的 usage（按需注入模拟缓存字段）
    let mut start_usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0
    });
    if let Some((settings, ttl)) = cache_sim_ctx {
        if let Some(cache_usage) = cache_sim::simulate(settings, *ttl, input_tokens) {
            cache_usage.apply_to_usage(&mut start_usage);
        }
    }

    // 1. message_start
    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "usage": start_usage
            }
        }),
    ));

    // 2. content_block_start (server_tool_use, index 0)
    // server_tool_use 是服务端工具，input 在 content_block_start 中一次性完整发送。
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "id": tool_use_id,
                "type": "server_tool_use",
                "name": "web_search",
                "input": {"query": query}
            }
        }),
    ));

    // 3. content_block_stop (server_tool_use)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 0
        }),
    ));

    // 4. content_block_start (web_search_tool_result, index 1)
    // 对齐官方：带 tool_use_id 与 caller。
    let search_content = build_search_result_content(&search_results);
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_content,
                "caller": {"type": "direct"}
            }
        }),
    ));

    // 5. content_block_stop (web_search_tool_result)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 1
        }),
    ));

    // 6. content_block_start (text, index 2)
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    // 7. content_block_delta (text_delta) - 模型综合答案，分块发送
    let chunk_size = 100;
    for chunk in answer.chars().collect::<Vec<_>>().chunks(chunk_size) {
        let text: String = chunk.iter().collect();
        events.push(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ));
    }

    // 8. content_block_stop (text)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 2
        }),
    ));

    // 9. message_delta
    let output_tokens = (answer.len() as i32 + 3) / 4; // 简单估算
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn"
            },
            "usage": {
                "output_tokens": output_tokens,
                "server_tool_use": {
                    "web_search_requests": 1,
                    "web_fetch_requests": 0
                }
            }
        }),
    ));

    // 10. message_stop
    events.push(SseEvent::new(
        "message_stop",
        json!({
            "type": "message_stop"
        }),
    ));

    events
}

/// 构建 web_search_tool_result 的 content 数组
///
/// 流式与非流式响应共用此逻辑。`encrypted_content` 为 base64(snippet)
/// （官方为加密 token，代理无法复刻）；`page_age` 为相对时间。
fn build_search_result_content(search_results: &Option<WebSearchResults>) -> Vec<serde_json::Value> {
    match search_results {
        Some(results) => results
            .results
            .iter()
            .map(|r| {
                let page_age = r.published_date.and_then(relative_page_age);
                let encrypted_content =
                    base64_encode(r.snippet.clone().unwrap_or_default().as_bytes());
                json!({
                    "type": "web_search_result",
                    "title": r.title,
                    "url": r.url,
                    "encrypted_content": encrypted_content,
                    "page_age": page_age
                })
            })
            .collect(),
        None => vec![],
    }
}

/// 生成 WebSearch 非流式 JSON 响应
///
/// 聚合为单个 Anthropic message 对象：
/// server_tool_use + web_search_tool_result + text（模型综合答案）。
fn create_websearch_message(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: &Option<WebSearchResults>,
    answer: &str,
    input_tokens: i32,
    cache_sim_ctx: &CacheSimCtx,
) -> serde_json::Value {
    let search_content = build_search_result_content(search_results);
    let output_tokens = (answer.len() as i32 + 3) / 4; // 简单估算

    let mut usage = json!({
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "server_tool_use": {
            "web_search_requests": 1,
            "web_fetch_requests": 0
        }
    });
    if let Some((settings, ttl)) = cache_sim_ctx {
        if let Some(cache_usage) = cache_sim::simulate(settings, *ttl, input_tokens) {
            cache_usage.apply_to_usage(&mut usage);
        }
    }

    json!({
        "id": generate_message_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [
            {
                "id": tool_use_id,
                "type": "server_tool_use",
                "name": "web_search",
                "input": {"query": query}
            },
            {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_content,
                "caller": {"type": "direct"}
            },
            {
                "type": "text",
                "text": answer
            }
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": usage
    })
}

/// 生成搜索结果摘要
fn generate_search_summary(query: &str, results: &Option<WebSearchResults>) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if let Some(results) = results {
        for (i, result) in results.results.iter().enumerate() {
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary.push_str("\nPlease note that these are web search results and may not be fully accurate or up-to-date.");

    summary
}

/// 处理 WebSearch 请求（完整模型驱动）
///
/// 流程：模型生成搜索词 → MCP 搜索 → 模型综合答案 → 按 Anthropic 格式输出。
/// 两个模型调用步骤都有回退，保证不至于整体失败。
pub async fn handle_websearch_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    input_tokens: i32,
    cache_sim_ctx: CacheSimCtx,
) -> Response {
    // 0. 用户意图（用于综合答案）
    let user_intent = match latest_user_text(payload) {
        Some(t) => t,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索意图",
                )),
            )
                .into_response();
        }
    };

    // 1. 模型生成带日期的搜索词（失败回退用户原话）
    let query = generate_search_query(&provider, payload).await;
    tracing::info!(query = %query, "WebSearch: 生成搜索词");

    // 2. 创建 MCP 请求并调用 Kiro MCP 搜索
    let (tool_use_id, mcp_request) = create_mcp_request(&query);
    let search_results = match call_mcp_api(&provider, &mcp_request).await {
        Ok(response) => parse_search_results(&response),
        Err(e) => {
            tracing::warn!("MCP API 调用失败: {}", e);
            None
        }
    };

    // 3. 模型综合答案（失败回退本地摘要）
    let answer = synthesize_answer(&provider, &payload.model, &user_intent, &search_results).await;

    // 4. 按请求的 stream 字段生成对应响应
    let model = payload.model.clone();

    if payload.stream {
        let stream = create_websearch_sse_stream(
            model,
            query,
            tool_use_id,
            search_results,
            answer,
            input_tokens,
            cache_sim_ctx,
        );

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        let body = create_websearch_message(
            &model,
            &query,
            &tool_use_id,
            &search_results,
            &answer,
            input_tokens,
            &cache_sim_ctx,
        );
        (StatusCode::OK, Json(body)).into_response()
    }
}

/// 调用 Kiro MCP API
async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
) -> anyhow::Result<McpResponse> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!("MCP request: {}", request_body);

    let response = provider.call_mcp(&request_body).await?;

    let body = response.text().await?;
    tracing::debug!("MCP response: {}", body);

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if let Some(ref error) = mcp_response.error {
        anyhow::bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }

    Ok(mcp_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_web_search_tool_only_one() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(8),
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        assert!(has_web_search_tool(&req));
    }

    #[test]
    fn test_has_web_search_tool_multiple_tools() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: Some("web_search_20250305".to_string()),
                    name: "web_search".to_string(),
                    description: String::new(),
                    input_schema: Default::default(),
                    max_uses: Some(8),
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        // 多个工具时不应该被识别为纯 websearch 请求
        assert!(!has_web_search_tool(&req));
    }

    #[test]
    fn test_extract_search_query_with_prefix() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "Perform a web search for the query: rust latest version 2026"
                }]),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        // 前缀应该被去除
        assert_eq!(query, Some("rust latest version 2026".to_string()));
    }

    #[test]
    fn test_extract_search_query_plain_text() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("What is the weather today?"),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        assert_eq!(query, Some("What is the weather today?".to_string()));
    }

    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        // tool_use_id 采用官方格式 srvtoolu_01 + 22 位 base62
        assert!(tool_use_id.starts_with("srvtoolu_01"));
        let suffix = &tool_use_id["srvtoolu_".len()..];
        assert_eq!(suffix.len(), 24);
        assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}_{毫秒时间戳}_{8位}
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        let parts: Vec<&str> = suffix.split('_').collect();
        assert_eq!(parts.len(), 3, "应该有3个部分: 22位随机_时间戳_8位随机");

        // 第一部分: 22位大小写字母和数字
        assert_eq!(parts[0].len(), 22);
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));

        // 第二部分: 毫秒时间戳
        assert!(parts[1].parse::<i64>().is_ok());

        // 第三部分: 8位小写字母和数字
        assert_eq!(parts[2].len(), 8);
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string(),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results(&response);
        assert!(results.is_some());
        let results = results.unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }

    #[test]
    fn test_create_websearch_message() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("today news".to_string()),
            error: None,
        };

        let msg = create_websearch_message(
            "claude-sonnet-4-6",
            "today news",
            "srvtoolu_01abcdefghijklmnopqrstuv",
            &Some(results),
            "综合答案：今天的新闻包含 Test Result。",
            42,
            &None,
        );

        // 顶层字段
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["model"], "claude-sonnet-4-6");
        assert_eq!(msg["stop_reason"], "end_turn");

        // 官方格式的 message id：msg_01 + 22 位 base62
        let id = msg["id"].as_str().unwrap();
        assert!(id.starts_with("msg_01"));
        assert_eq!(id["msg_".len()..].len(), 24);
        assert!(id["msg_".len()..].chars().all(|c| c.is_ascii_alphanumeric()));

        // content 数组：server_tool_use + web_search_tool_result + text
        let content = msg["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "server_tool_use");
        assert_eq!(content[0]["name"], "web_search");
        assert_eq!(content[0]["id"], "srvtoolu_01abcdefghijklmnopqrstuv");
        assert_eq!(content[0]["input"]["query"], "today news");
        // web_search_tool_result 带 tool_use_id 与 caller
        assert_eq!(content[1]["type"], "web_search_tool_result");
        assert_eq!(content[1]["tool_use_id"], "srvtoolu_01abcdefghijklmnopqrstuv");
        assert_eq!(content[1]["caller"]["type"], "direct");
        assert_eq!(content[1]["content"][0]["type"], "web_search_result");
        assert_eq!(content[1]["content"][0]["title"], "Test Result");
        // encrypted_content 为 base64(snippet)
        assert_eq!(
            content[1]["content"][0]["encrypted_content"],
            base64_encode("This is a test snippet".as_bytes())
        );
        assert_eq!(content[2]["type"], "text");
        assert!(content[2]["text"].as_str().unwrap().contains("Test Result"));

        // usage：含 server_tool_use，无缓存字段（cache_sim_ctx = None）
        assert_eq!(msg["usage"]["input_tokens"], 42);
        assert_eq!(msg["usage"]["server_tool_use"]["web_search_requests"], 1);
        assert_eq!(msg["usage"]["server_tool_use"]["web_fetch_requests"], 0);
        assert!(msg["usage"].get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn test_websearch_message_with_cache_sim() {
        use crate::model::cache_sim::{CacheSimSettings, CacheTtl};
        let ctx = Some((
            CacheSimSettings {
                enabled: true,
                creation_ratio: 0.2,
                hit_ratio: 0.7,
                cacheable_ratio: 1.0,
            },
            CacheTtl::FiveMin,
        ));
        let msg = create_websearch_message(
            "claude-sonnet-4-6",
            "q",
            "srvtoolu_01abcdefghijklmnopqrstuv",
            &None,
            "ans",
            1000,
            &ctx,
        );
        assert_eq!(msg["usage"]["input_tokens"], 101);
        assert_eq!(msg["usage"]["cache_creation_input_tokens"], 200);
        assert_eq!(msg["usage"]["cache_read_input_tokens"], 699);
        assert_eq!(msg["usage"]["cache_creation"]["ephemeral_5m_input_tokens"], 200);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_relative_page_age() {
        let now = chrono::Utc::now();
        let two_hours_ago = (now - chrono::Duration::hours(2)).timestamp_millis();
        assert_eq!(relative_page_age(two_hours_ago).as_deref(), Some("2 hours ago"));

        let three_days_ago = (now - chrono::Duration::days(3)).timestamp_millis();
        assert_eq!(relative_page_age(three_days_ago).as_deref(), Some("3 days ago"));

        let two_weeks_ago = (now - chrono::Duration::days(14)).timestamp_millis();
        assert_eq!(relative_page_age(two_weeks_ago).as_deref(), Some("2 weeks ago"));
    }

    #[test]
    fn test_generate_websearch_events_format() {
        let events = generate_websearch_events(
            "claude-sonnet-4-6",
            "today news",
            "srvtoolu_01abcdefghijklmnopqrstuv",
            None,
            "answer text",
            42,
            &None,
        );

        // message_start 在最前，message_stop 在最后
        assert_eq!(events.first().unwrap().event, "message_start");
        assert_eq!(events.last().unwrap().event, "message_stop");

        // server_tool_use 块 index 0
        let stu = events
            .iter()
            .find(|e| e.data["content_block"]["type"] == "server_tool_use")
            .expect("应有 server_tool_use 块");
        assert_eq!(stu.data["index"], 0);
        assert_eq!(stu.data["content_block"]["id"], "srvtoolu_01abcdefghijklmnopqrstuv");

        // web_search_tool_result 带 tool_use_id 与 caller
        let wstr = events
            .iter()
            .find(|e| e.data["content_block"]["type"] == "web_search_tool_result")
            .expect("应有 web_search_tool_result 块");
        assert_eq!(wstr.data["content_block"]["tool_use_id"], "srvtoolu_01abcdefghijklmnopqrstuv");
        assert_eq!(wstr.data["content_block"]["caller"]["type"], "direct");
    }
}
