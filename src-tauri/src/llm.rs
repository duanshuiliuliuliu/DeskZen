use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// DeepSeek（OpenAI 兼容）配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    /// 视觉模型：用于“问屏幕”等截图问答。为空时回退到 `model`。
    #[serde(default)]
    pub vision_model: String,
    pub api_key: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-flash".into(),
            vision_model: "deepseek-v4-flash-vision-exp".into(),
            api_key: String::new(),
        }
    }
}

/// 消息内容：既可以是纯文本（兼容旧配置），也可以是多模态内容数组（文本 + 图片）。
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

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: LlmMessage,
}

/// 读取 LLM 配置：优先级 环境变量 DESKZEN_DEEPSEEK_KEY > AppData 下 llm.json > 默认值
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
                if !disk.vision_model.is_empty() {
                    cfg.vision_model = disk.vision_model;
                }
                if !disk.api_key.is_empty() {
                    cfg.api_key = disk.api_key;
                }
            }
        }
    }
    if let Ok(key) = std::env::var("DESKZEN_DEEPSEEK_KEY") {
        if !key.is_empty() {
            cfg.api_key = key;
        }
    }
    cfg
}

/// 调用 chat completions，返回模型回复文本
pub async fn chat_completion(
    cfg: &LlmConfig,
    model: &str,
    messages: &[LlmMessage],
) -> Result<String, String> {
    if cfg.api_key.is_empty() {
        return Err("尚未配置 DeepSeek API Key。请在 AppData/com.deskzen.app/llm.json 或环境变量 DESKZEN_DEEPSEEK_KEY 中配置。".into());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
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
        .map_err(|e| format!("请求 DeepSeek 失败：{e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取响应失败：{e}"))?;
    if !status.is_success() {
        return Err(format!("DeepSeek 返回错误（{status}）：{}", truncate(&text, 300)));
    }
    let parsed: ChatResponse =
        serde_json::from_str(&text).map_err(|e| format!("响应解析失败：{e}"))?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content.as_text().trim().to_string())
        .ok_or_else(|| "DeepSeek 返回了空响应".into())
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
            model: "deepseek-v4-flash-vision-exp",
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

    /// 纯文本消息（历史记录里常见）应序列化为字符串，保持向后兼容。
    #[test]
    fn content_text_serializes_as_string() {
        let msg = LlmMessage {
            role: "user".into(),
            content: Content::Text("你好".into()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"], serde_json::json!("你好"));
    }

    /// 真实调用 DeepSeek，验证网关代码路径（读取 AppData 中的 llm.json，不硬编码 key）
    #[tokio::test]
    async fn chat_completion_works() {
        let path = std::path::Path::new(r"C:\Users\Baosong.Nan\AppData\Roaming\com.deskzen.app\llm.json");
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
            .expect("DeepSeek 调用失败");
        assert!(!reply.trim().is_empty());
        println!("reply: {reply}");
    }
}
