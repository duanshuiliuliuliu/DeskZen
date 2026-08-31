use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use chrono::Timelike;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// 一个角色的完整配置（MVP 先从内置 JSON 加载）
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PersonaConfig {
    pub id: String,
    pub name: String,
    pub system_prompt: SystemPromptConfig,
    pub states: HashMap<String, StateConfig>,
    pub schedule: Vec<ScheduleEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemPromptConfig {
    pub definition: String,
    pub reply_style: String,
    pub state_guidelines: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StateConfig {
    /// 状态的中文显示名（悬停标签 / 对话窗头部）
    pub label: String,
    /// spritesheet 中该状态所在的行
    pub row: u32,
    /// 该状态的帧数
    pub frames: u32,
    /// 每帧时长（毫秒）
    pub frame_ms: u64,
    /// 该状态下的气泡文本池
    pub bubbles: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleEntry {
    /// "HH:MM"
    pub start: String,
    /// "HH:MM"，支持跨午夜（end < start）
    pub end: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateChanged {
    pub state: String,
    pub previous: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BubbleEvent {
    pub state: String,
    pub text: String,
}

const PERSONA_JSON: &str = include_str!("../personas/dog.json");

/// 生活状态引擎：根据本地时间与 persona 作息表计算当前状态，
/// 变化时向所有窗口广播事件。只依赖 Rust 进程，不依赖 WebView 存活。
pub struct StateEngine {
    app: AppHandle,
    persona: PersonaConfig,
    last_state: Arc<Mutex<Option<String>>>,
}

impl StateEngine {
    pub fn new(app: AppHandle) -> Self {
        let persona: PersonaConfig =
            serde_json::from_str(PERSONA_JSON).expect("persona 配置解析失败");
        Self {
            app,
            persona,
            last_state: Arc::new(Mutex::new(None)),
        }
    }

    /// 当前状态（按本地时间实时计算）
    pub fn current_state(&self) -> String {
        state_at(&self.persona, chrono::Local::now())
    }

    /// 启动即广播一次当前状态与开场气泡
    pub fn broadcast_state(&self) {
        let state = self.current_state();
        let _ = self.app.emit(
            "state-changed",
            StateChanged {
                state: state.clone(),
                previous: String::new(),
            },
        );
        if let Some(cfg) = self.persona.states.get(&state) {
            if let Some(text) = pick(&cfg.bubbles) {
                let _ = self.app.emit("bubble", BubbleEvent { state, text });
            }
        }
    }

    /// 后台节拍线程：每 30 秒检查一次状态，变化时广播并弹出对应气泡
    pub fn start(&self) {
        let app = self.app.clone();
        let persona = self.persona.clone();
        let last_state = Arc::clone(&self.last_state);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(30));
            let state = state_at(&persona, chrono::Local::now());
            let mut last = last_state.lock().unwrap();
            if last.as_deref() != Some(state.as_str()) {
                let previous = last.clone().unwrap_or_default();
                *last = Some(state.clone());
                let _ = app.emit(
                    "state-changed",
                    StateChanged {
                        state: state.clone(),
                        previous,
                    },
                );
                if let Some(cfg) = persona.states.get(&state) {
                    if let Some(text) = pick(&cfg.bubbles) {
                        let _ = app.emit("bubble", BubbleEvent { state, text });
                    }
                }
            }
        });
    }

    pub fn persona(&self) -> &PersonaConfig {
        &self.persona
    }
}

#[tauri::command]
pub fn get_persona_config(engine: tauri::State<'_, StateEngine>) -> PersonaConfig {
    engine.persona().clone()
}

#[tauri::command]
pub fn get_current_state(engine: tauri::State<'_, StateEngine>) -> String {
    engine.current_state()
}

fn state_at(persona: &PersonaConfig, now: chrono::DateTime<chrono::Local>) -> String {
    let mins = now.hour() as u32 * 60 + now.minute() as u32;
    for entry in &persona.schedule {
        if let (Some(start), Some(end)) = (parse_mins(&entry.start), parse_mins(&entry.end)) {
            let hit = if start <= end {
                mins >= start && mins < end
            } else {
                // 跨午夜段：00:00~end 归入前一段
                mins >= start || mins < end
            };
            if hit {
                return entry.state.clone();
            }
        }
    }
    // 兜底：任意一个已配置状态
    persona
        .states
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "Awake".into())
}

/// 根据 persona 定义与当前状态组装 LLM system prompt
pub fn build_system_prompt(persona: &PersonaConfig, state: &str) -> String {
    let sp = &persona.system_prompt;
    let label = persona
        .states
        .get(state)
        .map(|s| s.label.as_str())
        .unwrap_or(state);
    let guideline = sp
        .state_guidelines
        .get(state)
        .map(String::as_str)
        .unwrap_or("");
    format!(
        "【角色定义】\n{}\n\n【回复风格】\n{}\n\n【当前状态】\n角色当前处于“{}”（{}）状态。{}",
        sp.definition, sp.reply_style, label, state, guideline
    )
}

fn parse_mins(value: &str) -> Option<u32> {
    let (h, m) = value.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    Some(h * 60 + m)
}

/// 简易伪随机：按当前纳秒时间从文本池取一条，避免引入 rand 依赖
fn pick(list: &[String]) -> Option<String> {
    if list.is_empty() {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as usize;
    Some(list[nanos % list.len()].clone())
}
