use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// DeepSeek（OpenAI 兼容）配置
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
            model: "deepseek-v4-flash".into(),
            api_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
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
pub async fn chat_completion(cfg: &LlmConfig, messages: &[LlmMessage]) -> Result<String, String> {
    if cfg.api_key.is_empty() {
        return Err("尚未配置 DeepSeek API Key。请在 AppData/com.deskzen.app/llm.json 或环境变量 DESKZEN_DEEPSEEK_KEY 中配置。".into());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model: &cfg.model,
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
        .map(|c| c.message.content.trim().to_string())
        .ok_or_else(|| "DeepSeek 返回了空响应".into())
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
        let reply = chat_completion(&cfg, &messages)
            .await
            .expect("DeepSeek 调用失败");
        assert!(!reply.trim().is_empty());
        println!("reply: {reply}");
    }
}
