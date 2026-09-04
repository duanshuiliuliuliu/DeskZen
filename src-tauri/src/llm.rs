use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 复用全局 HTTP 客户端，避免每次请求都重建连接池与空闲管理。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .expect("HTTP 客户端创建失败")
    })
}

/// LLM（OpenAI 兼容）配置：可指向任意兼容接口。
/// 默认值仅为示例（DeepSeek），用户可在设置/llm.json 里改成 OpenAI、Ollama 等任意接口。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-flash-vision-exp".into(),
            api_key: String::new(),
        }
    }
}

/// 消息内容：纯文本或多模态内容数组（文本 + 图片）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    /// 提取消息中的纯文本部分（多模态时把各文本片段拼接）。
    pub fn as_text(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

impl From<String> for Content {
    fn from(v: String) -> Self {
        Content::Text(v)
    }
}

impl From<&str> for Content {
    fn from(v: &str) -> Self {
        Content::Text(v.to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: Content,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [LlmMessage],
    temperature: f32,
    max_tokens: u32,
    stream: bool,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct Choice {
    message: LlmMessage,
}

/// 读取 LLM 配置：优先级 环境变量 DESKZEN_API_KEY > AppData 下 llm.json > 默认值
pub fn load_config(app: &AppHandle) -> LlmConfig {
    let mut cfg = LlmConfig::default();
    if let Ok(dir) = app.path().app_config_dir() {
        let file = dir.join("llm.json");
        if let Ok(content) = std::fs::read_to_string(file) {
            if let Ok(disk) = serde_json::from_str::<LlmConfig>(&content) {
                if !disk.base_url.is_empty() {
                    cfg.base_url = disk.base_url;
                }
                if !disk.model.is_empty() {
                    cfg.model = disk.model;
                }
                if !disk.api_key.is_empty() {
                    cfg.api_key = disk.api_key;
                }
            }
        }
    }
    let key = std::env::var("DESKZEN_API_KEY").unwrap_or_default();
    if !key.is_empty() {
        cfg.api_key = key;
    }
    cfg
}

/// 调用 chat completions，返回模型回复文本
#[allow(dead_code)]
pub async fn chat_completion(
    cfg: &LlmConfig,
    model: &str,
    messages: &[LlmMessage],
) -> Result<String, String> {
    if cfg.api_key.is_empty() {
        return Err("尚未配置 API Key。请在 AppData/com.deskzen.app/llm.json 或环境变量 DESKZEN_API_KEY 中配置。".into());
    }
    let client = http_client();
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model,
        messages,
        temperature: 0.8,
        max_tokens: 512,
        stream: false,
    };
    let resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 LLM 接口失败：{e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!("LLM 接口返回错误（{status}）：{}", truncate(&text, 300)));
    }
    let parsed: ChatResponse =
        serde_json::from_str(&text).map_err(|e| format!("响应解析失败：{e}"))?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.as_text().trim().to_string())
        .ok_or_else(|| "LLM 返回了空响应".into())
}

/// 流式调用 chat completions：以 SSE 逐段回调增量文本，返回完整累积文本。
///
/// 请求体带 `stream: true`，用 `resp.chunk()` 分段读响应体（无需依赖 `stream` 特性，
/// 该特性所需的 `tokio-util` 在当前离线环境无法下载，`chunk()` 与之等价且更轻量）。
pub async fn chat_completion_stream(
    cfg: &LlmConfig,
    model: &str,
    messages: &[LlmMessage],
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    if cfg.api_key.is_empty() {
        return Err("尚未配置 API Key。请在 AppData/com.deskzen.app/llm.json 或环境变量 DESKZEN_API_KEY 中配置。".into());
    }
    let client = http_client();
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model,
        messages,
        temperature: 0.8,
        max_tokens: 512,
        stream: true,
    };
    let mut resp = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 LLM 接口失败：{e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp
            .text()
            .await
            .map_err(|e| format!("读取响应失败：{e}"))?;
        return Err(format!("LLM 接口返回错误（{status}）：{}", truncate(&text, 300)));
    }

    let mut full = String::new();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("读取流失败：{e}"))?
    {
        buf.extend_from_slice(&chunk);
        // 按行出队：SSE 用 \n 分隔，一行即一条 `data: {...}`；能保证跨 chunk 的 UTF-8 不被截断。
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(delta) = parse_sse_delta(&line) {
                on_delta(&delta);
                full.push_str(&delta);
            }
        }
    }
    // 处理结尾没有换行符的最后一行。
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        if let Some(delta) = parse_sse_delta(&line) {
            on_delta(&delta);
            full.push_str(&delta);
        }
    }
    Ok(full)
}

/// 解析一行 SSE 事件：返回增量文本；空行、`data: [DONE]`、无 content 的 delta 均返回 None。
fn parse_sse_delta(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let data = line.strip_prefix("data:")?;
    let data = data.trim();
    if data == "[DONE]" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    let content = value["choices"][0]["delta"]["content"].as_str()?;
    if content.is_empty() {
        return None;
    }
    Some(content.to_string())
}

/// 把一张截图（data URL）附加到最后一条用户消息上，构成多模态内容。
/// 若没有任何用户消息，则补一条默认的“看屏幕”用户消息。
pub fn attach_image_to_last_user(messages: &mut Vec<LlmMessage>, image_data_url: String) {
    for msg in messages.iter_mut().rev() {
        if msg.role != "user" {
            continue;
        }
        let text = msg.content.as_text();
        msg.content = Content::Parts(vec![
            ContentPart::Text { text },
            ContentPart::ImageUrl {
                image_url: ImageUrl { url: image_data_url },
            },
        ]);
        return;
    }
    messages.push(LlmMessage {
        role: "user".into(),
        content: Content::Parts(vec![
            ContentPart::Text {
                text: "请看看当前屏幕截图。".into(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl { url: image_data_url },
            },
        ]),
    });
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 多模态消息（文本 + 图片）应序列化为 OpenAI 兼容的 content 数组格式。
    #[test]
    fn content_serializes_as_multimodal() {
        let msg = LlmMessage {
            role: "user".into(),
            content: Content::Parts(vec![
                ContentPart::Text {
                    text: "这是什么应用？".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,xyz".into(),
                    },
                },
            ]),
        };
        let body = ChatRequest {
            model: "vision-model",
            messages: &[msg],
            temperature: 0.8,
            max_tokens: 512,
            stream: false,
        };
        let json = serde_json::to_vec(&body).unwrap();
        let s = String::from_utf8(json).unwrap();
        assert!(s.contains("\"type\":\"text\""), "缺少 text 片段: {s}");
        assert!(s.contains("\"type\":\"image_url\""), "缺少 image_url 片段: {s}");
        assert!(
            s.contains("\"url\":\"data:image/png;base64,xyz\""),
            "缺少 base64 图片: {s}"
        );
    }

    /// 纯文本消息应序列化为字符串。
    #[test]
    fn content_text_serializes_as_string() {
        let msg = LlmMessage {
            role: "user".into(),
            content: Content::Text("你好".into()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"], serde_json::json!("你好"));
    }

    /// SSE 行解析：兼容带空格的 `data:`、跨 chunk 的换行结尾，忽略 `[DONE]` 与空 delta。
    #[test]
    fn sse_delta_parses_content() {
        assert_eq!(
            parse_sse_delta("data: {\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}").as_deref(),
            Some("你好")
        );
        assert_eq!(
            parse_sse_delta("data:{\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n").as_deref(),
            Some(" world")
        );
        assert_eq!(parse_sse_delta("data: [DONE]"), None);
        assert_eq!(parse_sse_delta(""), None);
        assert_eq!(
            parse_sse_delta("data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}"),
            None
        );
    }

    /// 真实调用 LLM，验证网关代码路径（读取 AppData 中的 llm.json）。
    /// 需要真实配置与网络，默认忽略；可用 `cargo test -- --ignored` 手动运行。
    #[ignore]
    #[tokio::test]
    async fn chat_completion_works() {
        let appdata = std::env::var("APPDATA").expect("APPDATA 未设置");
        let path = std::path::Path::new(&appdata).join("com.deskzen.app\\llm.json");
        let content = std::fs::read_to_string(path).expect("llm.json 不存在，请先配置 API Key");
        let cfg: LlmConfig = serde_json::from_str(&content).expect("llm.json 解析失败");
        assert!(!cfg.api_key.is_empty(), "llm.json 中缺少 api_key");

        let messages = vec![
            LlmMessage {
                role: "system".into(),
                content: "You are a pixel puppy. Reply briefly.".into(),
            },
            LlmMessage {
                role: "user".into(),
                content: "Say hi in one short sentence".into(),
            },
        ];
        let reply = chat_completion(&cfg, &cfg.model, &messages)
            .await
            .expect("LLM 调用失败");
        assert!(!reply.trim().is_empty());
        println!("reply: {reply}");
    }
}
