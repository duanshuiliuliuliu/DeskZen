use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        {Arc, Condvar, Mutex},
    },
    thread,
    time::Duration,
};

use crate::llm::LlmMessage;
use chrono::Timelike;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// 固定大状态集合（跨角色通用契约）：角色只能使用这 6 个状态 id。
/// 其中只有 `routine` 必配；其余状态未配置时，运行时完全按 routine 处理。
pub(crate) const CANONICAL_STATES: &[&str] =
    &["routine", "focus", "active", "relax", "eat", "sleep"];
/// 必配的兜底状态 id（日常）
pub(crate) const FALLBACK_STATE: &str = "routine";
/// `acknowledge` 里"所有状态的默认反应"的键名
pub(crate) const DEFAULT_ACK_KEY: &str = "default";

/// 一个角色的完整配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PersonaConfig {
    pub id: String,
    pub name: String,
    /// 角色在窗口中的显示尺寸
    #[serde(default)]
    pub display_w: u32,
    #[serde(default)]
    pub display_h: u32,
    pub system_prompt: SystemPromptConfig,
    /// 状态：展示/说话配置 + 该状态的活动链（链内段按顺序播放，表达因果）
    pub states: HashMap<String, StateConfig>,
    /// 可复用的动作片段；每个片段可来自独立的横向 spritesheet。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub clips: HashMap<String, AnimationClipConfig>,
    /// 动作资源目录（可选）：动作没写 `spritesheet` 时按 `<clips_dir>/<动作id>.webp` 派生
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub clips_dir: String,
    /// 被用户注意到（鼠标凑近 / 点一下 / 开口说话）时的即时反应：键为状态 id 或 `default`。
    /// 反应步骤、触发来源差异、限频参数都在这里配（详见 [`AcknowledgeConfig`]）。
    #[serde(default, skip_serializing_if = "AcknowledgeConfig::is_empty")]
    pub acknowledge: AcknowledgeConfig,
    /// 内部需求/情绪：演化速率、把角色拉去某个状态的阈值、按标签调制链权重
    #[serde(default, skip_serializing_if = "crate::needs::NeedsConfig::is_default")]
    pub needs: crate::needs::NeedsConfig,
    pub schedule: ScheduleConfig,
}

impl PersonaConfig {
    /// 加载/导入后补齐**派生字段**，让配置文件里能省则省：
    /// - 段 `id` 缺省 → `<链id>#<段序号>`（事件里的 `scene_id`）
    /// - 段 `label` 缺省 → 该段首个动作的 label（窗口标题用）
    /// - 链 `label` 缺省 → 该链首段的 label
    /// - 动作 `spritesheet` 缺省 → `<clips_dir>/<动作id>.webp`
    pub(crate) fn finalize(&mut self) {
        // 先在只读阶段取好动作名，避免下面可变遍历时借用冲突
        let clip_labels: HashMap<String, String> = self
            .clips
            .iter()
            .map(|(id, clip)| {
                let label = if clip.label.is_empty() {
                    id.clone()
                } else {
                    clip.label.clone()
                };
                (id.clone(), label)
            })
            .collect();
        for cfg in self.states.values_mut() {
            for chain in cfg.chains.iter_mut() {
                // 单段链简写：steps → segments[0].steps
                if chain.segments.is_empty() && !chain.steps.is_empty() {
                    chain.segments.push(SegmentConfig {
                        // 单段链写在外面的 label 就是这一段的段名
                        label: std::mem::take(&mut chain.label),
                        steps: std::mem::take(&mut chain.steps),
                        ..Default::default()
                    });
                }
                for (index, segment) in chain.segments.iter_mut().enumerate() {
                    if segment.id.is_empty() {
                        segment.id = format!("{}#{index}", chain.id);
                    }
                    if segment.label.is_empty() {
                        segment.label = segment
                            .steps
                            .first()
                            .and_then(|step| clip_labels.get(&step.clip).cloned())
                            .unwrap_or_else(|| segment.id.clone());
                    }
                }
                if chain.label.is_empty() {
                    chain.label = chain
                        .segments
                        .first()
                        .map(|segment| segment.label.clone())
                        .unwrap_or_else(|| chain.id.clone());
                }
            }
        }
        if !self.clips_dir.is_empty() {
            let dir = self.clips_dir.trim_end_matches('/').to_string();
            for (clip_id, clip) in self.clips.iter_mut() {
                if clip.spritesheet.is_empty() {
                    clip.spritesheet = format!("{dir}/{clip_id}.webp");
                }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemPromptConfig {
    pub definition: String,
    pub reply_style: String,
}

/// 状态的话痨程度：决定该状态下环境气泡的期望间隔倍率
/// （chatty ×0.5 / normal ×1.0 / quiet ×2.0 / mute 不弹）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Talkativeness {
    Chatty,
    #[default]
    Normal,
    Quiet,
    Mute,
}

impl Talkativeness {
    /// 宽松解析：未知值按 normal 处理——persona.json 可能被手改，
    /// 不能因一个非法字段让整个角色解析失败。
    pub fn parse(s: &str) -> Self {
        match s {
            "chatty" => Self::Chatty,
            "quiet" => Self::Quiet,
            "mute" => Self::Mute,
            _ => Self::Normal,
        }
    }

    /// 该话痨档位对应的默认对话语气（对话 system prompt 用）。
    /// 状态没显式写 `tone` 时用它，避免"语气"和"话痨程度"两处配置各写一遍。
    pub fn chat_tone(self) -> &'static str {
        match self {
            Self::Chatty => "你心情不错，愿意多说几句，回复可以稍微活泼一些。",
            Self::Normal => "你愿意进行简短交流。",
            Self::Quiet => "你正在专注自己的事，只进行必要且简短的交流。",
            Self::Mute => "你处于恍惚状态，只能含糊地回应几句，甚至只说梦话。",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StateConfig {
    /// 状态的中文显示名（对话窗头部）
    pub label: String,
    /// 话痨程度："chatty"|"normal"|"quiet"|"mute"；缺省或未知值按 normal
    #[serde(default)]
    pub talkativeness: String,
    /// 对话语气（可选）：想特化这个状态的说话风格时才写；
    /// 缺省由 `talkativeness` 推导（见 `Talkativeness::chat_tone`）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tone: String,
    /// 期望说话间隔（分钟，可选）：不写则按 `talkativeness` 取默认值
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bubble_gap_min: Option<f64>,
    /// 该状态的活动：链内段按顺序播放（表达因果），链之间按 `weight` 选择
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainConfig>,
}

impl StateConfig {
    pub fn talkativeness(&self) -> Talkativeness {
        Talkativeness::parse(&self.talkativeness)
    }

    /// 对话语气：显式 `tone` 优先，否则按话痨档位推导
    pub fn chat_tone(&self) -> &str {
        if self.tone.is_empty() {
            self.talkativeness().chat_tone()
        } else {
            &self.tone
        }
    }
}

/// 权重缺省值：1
fn default_unit() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnimationClipConfig {
    /// 横向帧条资源路径；内置资源以 / 开头，导入角色为磁盘绝对路径。
    /// 不写则按 `<clips_dir>/<动作id>.webp` 派生（见 [`PersonaConfig::finalize`]）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spritesheet: String,
    /// 动作的中文说明（气泡生成提示词与调试用）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    /// 这个动作的画面描述：写清"角色正在做什么、什么心情"，
    /// AI 台词生成只依据它（不再依据状态级指引），保证台词贴着当前动作说。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub frames: u32,
    pub frame_ms: u64,
    /// 该动作专属的气泡文案池（优先于状态级文案，保证"说什么"和"演什么"一致）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bubbles: Vec<String>,
}

/// 一段连续表演：有名字、有起止。段是"最短保持 + 一段最多一句台词"的单位。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SegmentConfig {
    /// 段 id；不写则取 `<链id>#<段序号>`（事件里的 `scene_id`）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// 段名（窗口标题用）；不写则取本段首个动作的 label
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub steps: Vec<SceneStepConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SceneStepConfig {
    pub clip: String,
    /// 这一拍的目标时长（秒）：写数字表示固定，写 `[min,max]` 表示每遍随机取区间内一个值。
    /// 实际时长会按动作原生时长取整到整数个循环；**不写 = 只播一遍**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<SecondsSpec>,
    /// 这一拍出现在本次表演里的概率（0~1，缺省 1）：同一个段每遍的节拍数会略有不同
    #[serde(default = "default_chance")]
    pub chance: f64,
}

/// 目标时长：固定值或随机区间
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SecondsSpec {
    Fixed(u32),
    Range([u32; 2]),
}

impl SecondsSpec {
    /// 这一遍的目标毫秒数（区间则随机取）
    pub(crate) fn sample_ms(&self) -> i64 {
        match self {
            Self::Fixed(seconds) => *seconds as i64 * 1000,
            Self::Range([min, max]) => {
                let (min, max) = ((*min).max(1), (*max).max(1));
                rng_range_i64(min.min(max) as i64, max as i64) * 1000
            }
        }
    }
}

fn default_chance() -> f64 {
    1.0
}

impl Default for SceneStepConfig {
    fn default() -> Self {
        Self {
            clip: String::new(),
            seconds: None,
            chance: 1.0,
        }
    }
}

/// 一条有序链：链内段按配置顺序播放（表达因果，不洗牌），链之间按权重选择。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainConfig {
    pub id: String,
    /// 链名（可选）：不写则取首段的 label；只用于调试/日志，不影响播放
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default = "default_unit")]
    pub weight: u32,
    /// 链内的段（按数组顺序播放，表达因果）：一段 = 一次连续表演
    #[serde(default)]
    pub segments: Vec<SegmentConfig>,
    /// 单段链的简写：等价于 `segments: [{steps}]`（与 `segments` 二选一）。
    /// 大部分链只有一个段，这样写少一层嵌套。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<SceneStepConfig>,
    /// 标签：需求偏置（`needs.chain_bias`）按标签匹配，例如 "rest" / "social" / "explore"
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 触发约束：权重只表达"偏好"，因果与节制交给它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<ChainWhen>,
}

/// 一条链的触发约束。三项都不配 = 随时可选（保持旧行为）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChainWhen {
    /// 需要最近播过其中**任一**动作（clip id）才可选——用来表达因果，例如
    /// "战斗之后"这条链要求 `["fight"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_recent: Vec<String>,
    /// `requires_recent` 的有效期（分钟），缺省 30
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_min: Option<u32>,
    /// 这条链播放后的冷却（分钟）：冷却期内不再选它（避免"保养上瘾"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_min: Option<u32>,
    /// 每天最多出现几次（给"彩蛋"类链条用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_day: Option<u32>,
}

impl ChainWhen {
    pub(crate) fn within_minutes(&self) -> i64 {
        self.within_min.unwrap_or(30) as i64
    }
}

/// 「被用户注意到」时的即时反应配置。
///
/// 触发来源（kind）：`hover`（鼠标凑近）、`click`（单击）、`chat`（打开对话窗）、
/// `talk`（发消息）。解析优先级：**按状态覆盖 > 按来源覆盖 > 默认**；任何一层
/// 显式配成空数组都表示"不响应"（例如睡觉时被打扰不动）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AcknowledgeConfig {
    /// 所有来源、所有状态通用的反应
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default: Vec<SceneStepConfig>,
    /// 按来源覆盖
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub by_kind: HashMap<String, Vec<SceneStepConfig>>,
    /// 按状态覆盖（优先级高于来源）
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub by_state: HashMap<String, Vec<SceneStepConfig>>,
    /// 各来源的最小间隔（秒）；`default` 键为未列出来源兜底，缺省 15
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub cooldown_s: HashMap<String, i64>,
    /// 指针类事件（hover/click）之间的全局最小间隔（秒）。明确互动（chat/talk）
    /// 不受它限制——用户主动搭话时不该被"刚看过你"挡掉。
    #[serde(default = "default_global_cooldown_s")]
    pub global_cooldown_s: i64,
    /// 当前动作剩余不足这么多秒就不打断（刚打断就结束反而突兀）
    #[serde(default = "default_min_remaining_s")]
    pub min_remaining_s: i64,
}

fn default_global_cooldown_s() -> i64 {
    6
}

fn default_min_remaining_s() -> i64 {
    3
}

/// 各来源的默认最小间隔（秒）：鼠标蹭过窗口很常见，间隔拉大；
/// 点击/搭话是明确互动，可以快一点。persona 可用 `cooldown_s` 覆盖。
pub(crate) fn default_cooldown_secs(kind: &str) -> i64 {
    match kind {
        // 停在角色身上一次就够了：两分钟内不再因为鼠标经过而反复打断
        "hover" => 120,
        "click" => 8,
        "chat" => 10,
        "talk" => 8,
        _ => 15,
    }
}

impl Default for AcknowledgeConfig {
    fn default() -> Self {
        Self {
            default: Vec::new(),
            by_kind: HashMap::new(),
            by_state: HashMap::new(),
            cooldown_s: HashMap::new(),
            global_cooldown_s: default_global_cooldown_s(),
            min_remaining_s: default_min_remaining_s(),
        }
    }
}

/// 合法的触发来源；`chat` 是"打开对话窗"，`talk` 是"发出一条消息"
pub(crate) const ACK_KINDS: &[&str] = &["hover", "click", "chat", "talk"];

impl AcknowledgeConfig {
    pub(crate) fn is_empty(&self) -> bool {
        self.default.is_empty() && self.by_kind.is_empty() && self.by_state.is_empty()
    }

    /// 这次该播什么：状态覆盖 > 来源覆盖 > 默认；空 = 不响应
    pub(crate) fn steps_for(&self, state: &str, kind: &str) -> Option<&[SceneStepConfig]> {
        let steps = self
            .by_state
            .get(state)
            .or_else(|| self.by_kind.get(kind))
            .unwrap_or(&self.default);
        if steps.is_empty() {
            None
        } else {
            Some(steps)
        }
    }

    /// 该来源的最小间隔（秒），缺省 15
    pub(crate) fn cooldown_secs(&self, kind: &str) -> i64 {
        self.cooldown_s
            .get(kind)
            .or_else(|| self.cooldown_s.get(DEFAULT_ACK_KEY))
            .copied()
            .unwrap_or_else(|| default_cooldown_secs(kind))
            .max(0)
    }

    /// 指针类事件受全局间隔约束；chat/talk 作为明确互动不受限
    pub(crate) fn is_pointer_kind(kind: &str) -> bool {
        matches!(kind, "hover" | "click")
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScheduleConfig {
    /// 循环状态（逐状态时长）
    #[serde(default)]
    pub r#loop: Vec<LoopEntry>,
    /// 时间段配置：在指定时间段内固定为对应状态
    #[serde(default)]
    pub time: Vec<TimeSlot>,
}

impl ScheduleConfig {
    pub fn loop_entries(&self) -> &[LoopEntry] {
        &self.r#loop
    }

    pub fn time(&self) -> &[TimeSlot] {
        &self.time
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct LoopEntry {
    pub state: String,
    /// 该状态循环时长（分钟）
    #[serde(default)]
    pub duration: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimeSlot {
    /// "HH:MM"
    pub start: String,
    /// "HH:MM"，支持跨午夜（end < start）
    pub end: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateChanged {
    pub state: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BubbleEvent {
    pub text: String,
    /// 气泡展示时长（毫秒）：由后端按当前动作剩余时间给出，保证不会跨到下一个动作
    pub show_ms: u64,
}

/// 环境气泡调度状态（与状态引擎同生命周期，不持久化）
#[derive(Debug, Default)]
struct AmbientBubbles {
    /// 最早可再次说话的时刻（由期望间隔推导）
    earliest_at: Option<chrono::DateTime<chrono::Local>>,
    /// 当前动作实例内已排定的说话时刻；None 表示这个实例不说
    speak_at: Option<chrono::DateTime<chrono::Local>>,
    /// 上一条气泡实际弹出时刻（静默上限用）
    last_shown_at: Option<chrono::DateTime<chrono::Local>>,
    /// 最近一次用户发消息时刻（聊天后抑制窗口用）
    last_chat_at: Option<chrono::DateTime<chrono::Local>>,
    /// 洗牌袋：池键 -> 本轮剩余未弹文案（取完一轮后重洗）。池键为 `clip:<动作id>`，
    /// 文案只挂在动作上，所以换动作就是换池，去重也按动作各自进行。
    bags: HashMap<String, VecDeque<String>>,
    /// 各池键上一条弹出的文案（跨轮防重复）
    last_text: HashMap<String, String>,
    /// 当日计划的"开场白"：当天第一次开口优先说它，说完即清（一天只说一次）
    opening: Option<String>,
}

/// 气泡默认展示时长（毫秒）；实际下发时还会被当前动作剩余时间截短
const BUBBLE_DEFAULT_SHOW_MS: i64 = 6000;
/// 一个动作实例至少持续多久才有资格说话（毫秒）：更短的是过渡动作，不配台词
pub(crate) const SPEAK_MIN_ACTION_MS: i64 = 30_000;
/// 说话时刻落在动作实例的 [20%, 70%] 区间内
const SPEAK_WINDOW_START: f64 = 0.2;
const SPEAK_WINDOW_END: f64 = 0.7;
/// 启动 / 切换角色后的第一句话延迟范围（秒）
const FIRST_SPEAK_MIN_S: i64 = 30;
const FIRST_SPEAK_MAX_S: i64 = 90;
/// 聊天后的气泡抑制窗口（分钟）
const CHAT_SUPPRESS_MIN: i64 = 3;
/// 用户关注事件的限频状态
#[derive(Debug, Default)]
struct SeenState {
    last_any: Option<chrono::DateTime<chrono::Local>>,
    last_kind: HashMap<String, chrono::DateTime<chrono::Local>>,
}

/// 默认期望说话间隔（分钟）：按话痨档位；mute 返回 None（不说话）。
/// 状态可用 `bubble_gap_min` 覆盖。
fn default_gap_min(t: Talkativeness) -> Option<f64> {
    match t {
        Talkativeness::Chatty => Some(3.0),
        Talkativeness::Normal => Some(6.0),
        Talkativeness::Quiet => Some(15.0),
        Talkativeness::Mute => None,
    }
}

/// 静默上限（分钟）：2×E，且不超过 15 分钟。
fn max_silence_min(e: f64) -> f64 {
    (2.0 * e).min(15.0)
}

/// 一次间隔采样：落在 [0.6E, 1.4E] 内
fn sample_gap_min(e: f64) -> f64 {
    let min = (e * 0.6 * 60.0).round() as i64;
    let max = (e * 1.4 * 60.0).round() as i64;
    rng_range_i64(min, max) as f64 / 60.0
}

/// 计算一个动作实例内允许说话的窗口 `[from, to]`；None 表示这个实例不说。
///
/// - 动作实例 < 30 秒 → 过渡动作，不说话；
/// - 说话时刻落在实例的 20%~70% 区间，且这句话必须能在实例结束前说完；
/// - 不早于 `earliest_at`（间隔护栏）；静默超过上限时忽略间隔，直接取窗口左端。
fn speech_window(
    now: chrono::DateTime<chrono::Local>,
    started_at: chrono::DateTime<chrono::Local>,
    duration_ms: u64,
    earliest_at: Option<chrono::DateTime<chrono::Local>>,
    last_shown_at: Option<chrono::DateTime<chrono::Local>>,
    e_min: f64,
) -> Option<(
    chrono::DateTime<chrono::Local>,
    chrono::DateTime<chrono::Local>,
)> {
    if (duration_ms as i64) < SPEAK_MIN_ACTION_MS {
        return None;
    }
    let ends_at = started_at + chrono::Duration::milliseconds(duration_ms as i64);
    let line_ms = BUBBLE_DEFAULT_SHOW_MS.min(duration_ms as i64);
    let window_start = started_at
        + chrono::Duration::milliseconds((duration_ms as f64 * SPEAK_WINDOW_START) as i64);
    let window_end =
        started_at + chrono::Duration::milliseconds((duration_ms as f64 * SPEAK_WINDOW_END) as i64);
    let latest = window_end.min(ends_at - chrono::Duration::milliseconds(line_ms));
    // 从未说过话（启动 / 切角色后）：不受 20%~70% 偏好窗口限制，
    // grace 一到、只要当前实例能说完就开口（最坏也是下一个动作一开始说）。
    if last_shown_at.is_none() {
        let from = earliest_at.unwrap_or(window_start).max(started_at);
        let to = ends_at - chrono::Duration::milliseconds(line_ms);
        return (from <= to).then_some((from, to));
    }
    // 说过话之后：静默超过上限 → 忽略间隔、取偏好窗口左端；否则不早于 earliest_at。
    let silence_too_long = last_shown_at
        .map(|t| (now - t).num_seconds() as f64 / 60.0 >= max_silence_min(e_min))
        .unwrap_or(false);
    let from = if silence_too_long {
        window_start
    } else {
        earliest_at.unwrap_or(window_start).max(window_start)
    };
    (from <= latest).then_some((from, latest))
}

/// 缩放变化后广播给前端的角色显示尺寸（已乘 zoom 的最终 pixel 尺寸）
#[derive(Debug, Clone, Serialize)]
pub struct ZoomChanged {
    pub w: u32,
    pub h: u32,
}

/// 发给前端的角色视图：只含渲染需要的字段。
/// 动作（clips）与场景（scenes）留在后端：场景由后端调度，动作参数随 `playback` 事件下发。
#[derive(Debug, Clone, Serialize)]
pub struct PersonaView {
    pub id: String,
    pub name: String,
    /// 已乘全局缩放的显示尺寸
    pub display_w: u32,
    pub display_h: u32,
    /// 状态 id -> { label, talkativeness, tone }
    pub states: HashMap<String, StateConfig>,
}

/// 当前播放快照：对话时注入 system prompt，让模型的回答锚定在“画面里正在演的动作”上。
/// 没有它时模型会自行编造动作（例如问“在干什么”回答“磨剑”，而动作集里根本没有磨剑）。
#[derive(Debug, Clone, Serialize)]
pub struct ActivityView {
    pub state: String,
    pub scene_id: String,
    pub scene_label: String,
    pub clip: String,
    pub clip_label: String,
    pub clip_description: String,
}

/// 内置角色：id -> 配置文件（编译期内嵌，运行时切换）
const EMBEDDED_PERSONAS: &[(&str, &str)] = &[(
    "link",
    include_str!("../../resources/characters/link/persona.json"),
)];

/// 生活状态引擎：根据本地时间与当前 persona 作息表计算状态，
/// 变化时向所有窗口广播事件。只依赖 Rust 进程，不依赖 WebView 存活。
pub struct StateEngine {
    app: AppHandle,
    personas: Arc<Mutex<HashMap<String, Arc<PersonaConfig>>>>,
    /// 当前角色配置；用 Arc 包一层，热路径（动作结束/气泡）克隆引用计数而不是整份配置
    persona: Arc<Mutex<Arc<PersonaConfig>>>,
    last_state: Arc<Mutex<Option<String>>>,
    /// 当前角色的有效显示尺寸缓存：拖动/贴气泡时频繁读取，避免每次都克隆整个 persona
    /// 或对导入角色读磁盘做 image_dimensions。
    display_size: Arc<Mutex<(u32, u32)>>,
    /// 全局缩放偏好（含 zoom）；放在引擎侧便于 switch_persona / get_persona_config 直接用。
    prefs: Arc<Mutex<crate::prefs::Prefs>>,
    /// 每日 AI 气泡缓存（当前角色的）：气泡广播时优先取当日生成文案，取不到回退原配置。
    gen_bubbles: Arc<Mutex<crate::genbubble::GenState>>,
    /// 环境气泡调度状态：按动作实例排一句话、期望间隔与洗牌袋（见 reschedule_speech）
    ambient: Arc<Mutex<AmbientBubbles>>,
    /// 场景播放调度：由后端决定"当前播哪个场景的哪个动作"，前端按事件渲染
    playback: Arc<Mutex<crate::playback::Playback>>,
    /// 用户关注事件（凑近/点击/搭话）的限频状态
    seen: Arc<Mutex<SeenState>>,
    /// 内部需求/情绪（当前角色的）：随时间和状态演化，重启后按离线时长补算
    needs: Arc<Mutex<crate::needs::NeedsState>>,
    /// 最近一次下发的链 id（用于判断"换了一条新链"，给无聊降温）
    last_chain: Arc<Mutex<Option<String>>>,
    /// 当日计划（当前角色的）：每天生成一次，调制链权重 + 当天第一次开口说出来
    plan: Arc<Mutex<crate::plan::PlanState>>,
    /// 当日计划生成任务是否在跑（防止重复起任务）
    pub(crate) plan_generating: Arc<AtomicBool>,
    /// 每日气泡生成任务是否在跑（防止重复起任务）
    pub(crate) generating: Arc<AtomicBool>,
    /// 后台节拍线程的唤醒信号：switch_persona 换角色后 notify，
    /// 让线程立即醒来重算下一次切换时刻，避免睡到旧的 next_transition_at。
    wake_lock: Arc<Mutex<()>>,
    wake_cond: Arc<Condvar>,
}

impl StateEngine {
    pub fn new(app: AppHandle) -> Self {
        let mut personas = HashMap::new();
        for (id, json) in EMBEDDED_PERSONAS {
            let mut cfg: PersonaConfig = serde_json::from_str(json).expect("persona 配置解析失败");
            cfg.finalize();
            personas.insert((*id).to_string(), Arc::new(cfg));
        }
        // 加载用户导入的角色（持久化在用户数据目录；clips/scenes 格式由导入时校验）
        if let Ok(chars_dir) = Self::characters_dir_for(&app) {
            if let Ok(entries) = std::fs::read_dir(&chars_dir) {
                for entry in entries.flatten() {
                    let cfg_path = entry.path().join("persona.json");
                    let Ok(json) = std::fs::read_to_string(&cfg_path) else {
                        continue;
                    };
                    // 只加载结构完整的角色（与导入用同一套判据）；坏包跳过并说明原因，
                    // 避免注册进去后某些状态静默不播
                    match serde_json::from_str::<PersonaConfig>(&json) {
                        Ok(mut cfg) => {
                            cfg.finalize();
                            match validate_persona_structure(&cfg) {
                                Ok(()) => {
                                    personas.insert(cfg.id.clone(), Arc::new(cfg));
                                }
                                Err(reason) => {
                                    eprintln!("跳过角色目录 {}：{reason}", entry.path().display())
                                }
                            }
                        }
                        Err(error) => eprintln!(
                            "跳过角色目录 {}：persona.json 解析失败（{error}）",
                            entry.path().display()
                        ),
                    }
                }
            }
        }
        let persona = personas.get("link").cloned().expect("缺少默认角色 link");
        let prefs = crate::prefs::load_prefs(&app);
        let needs = crate::needs::load(&app, "link", &persona.needs);
        let gen_cache = crate::genbubble::load(&app, "link");
        let plan_cache = crate::plan::load(&app, "link");
        let mut display = effective_display_size_zoomed(&persona, prefs.zoom);
        // 启动即按显示器工作区等比适配，避免离谱 display / 高 zoom 时窗口出屏（取不到工作区则不钳制）。
        if let Some((mw, mh)) = work_area_content_limit(&app) {
            display = fit_display_size_proportional(display.0, display.1, mw, mh);
        }
        Self {
            app,
            personas: Arc::new(Mutex::new(personas)),
            persona: Arc::new(Mutex::new(persona)),
            last_state: Arc::new(Mutex::new(None)),
            display_size: Arc::new(Mutex::new(display)),
            gen_bubbles: Arc::new(Mutex::new(crate::genbubble::GenState {
                persona_id: "link".into(),
                cache: gen_cache,
            })),
            ambient: Arc::new(Mutex::new(AmbientBubbles::default())),
            playback: Arc::new(Mutex::new(crate::playback::Playback::default())),
            seen: Arc::new(Mutex::new(SeenState::default())),
            needs: Arc::new(Mutex::new(needs)),
            last_chain: Arc::new(Mutex::new(None)),
            plan: Arc::new(Mutex::new(crate::plan::PlanState {
                persona_id: "link".to_string(),
                cache: plan_cache,
            })),
            plan_generating: Arc::new(AtomicBool::new(false)),
            generating: Arc::new(AtomicBool::new(false)),
            prefs: Arc::new(Mutex::new(prefs)),
            wake_lock: Arc::new(Mutex::new(())),
            wake_cond: Arc::new(Condvar::new()),
        }
    }

    /// 当前状态：按本地时间（硬时段 > 需求 > 循环）实时计算
    pub fn current_state(&self) -> String {
        let persona = crate::util::lock(&self.persona);
        self.resolve_state(&persona, chrono::Local::now())
    }

    /// 结合日程与内部需求得出最终状态
    fn resolve_state(
        &self,
        persona: &PersonaConfig,
        now: chrono::DateTime<chrono::Local>,
    ) -> String {
        // 需求快照在锁外取，避免与 persona 形成嵌套锁
        let needs = crate::util::lock(&self.needs).clone();
        runtime_state(persona, &automatic_state(persona, now_minutes(&now), &needs))
    }

    /// 当前角色配置（克隆）
    pub fn persona(&self) -> Arc<PersonaConfig> {
        Arc::clone(&crate::util::lock(&self.persona))
    }

    pub fn persona_id(&self) -> String {
        crate::util::lock(&self.persona).id.clone()
    }

    /// 当前角色的有效显示尺寸（缓存值），供贴气泡等高频 UI 计算使用。
    pub fn display_size(&self) -> (u32, u32) {
        *crate::util::lock(&self.display_size)
    }

    /// 当前缩放偏好（克隆），供 set_zoom / get_prefs 读取。
    pub fn prefs(&self) -> crate::prefs::Prefs {
        crate::util::lock(&self.prefs).clone()
    }

    // ---- 每日 AI 气泡：缓存读写与触发（见 genbubble.rs）----

    /// 某动作今天是否已有生成台词
    pub(crate) fn gen_has_today(&self, clip_id: &str) -> bool {
        crate::genbubble::today_pool(&self.gen_bubbles, clip_id).is_some()
    }

    /// 某动作当天是否已标记失败（当天不再重试）
    pub(crate) fn gen_failed(&self, persona_id: &str, clip_id: &str) -> bool {
        let g = crate::util::lock(&self.gen_bubbles);
        g.persona_id == persona_id && g.cache.failed.iter().any(|s| s == clip_id)
    }

    /// 生成历史快照（查重用）
    pub(crate) fn gen_history(&self) -> Vec<String> {
        crate::util::lock(&self.gen_bubbles).cache.history.clone()
    }

    /// 记录一个动作当日生成成功（一批台词）：更新内存缓存并落盘
    /// （切换角色后到达的迟到结果直接丢弃）
    pub(crate) fn record_gen_bubble(&self, persona_id: &str, clip_id: &str, texts: &[String]) {
        if texts.is_empty() {
            return;
        }
        let (cache, changed) = {
            let mut g = crate::util::lock(&self.gen_bubbles);
            if g.persona_id != persona_id {
                return;
            }
            g.cache.date = crate::genbubble::today_str();
            g.cache.by_clip.insert(clip_id.to_string(), texts.to_vec());
            g.cache.history.extend(texts.iter().cloned());
            let keep = g
                .cache
                .history
                .len()
                .saturating_sub(crate::genbubble::HISTORY_KEEP);
            if keep > 0 {
                g.cache.history.drain(..keep);
            }
            (g.cache.clone(), true)
        };
        if changed {
            let _ = crate::genbubble::save(&self.app, persona_id, &cache);
        }
    }

    /// 记录一个动作当日生成失败（不写历史、只标记，当天回退原配置）
    pub(crate) fn record_gen_failure(&self, persona_id: &str, clip_id: &str) {
        let (cache, changed) = {
            let mut g = crate::util::lock(&self.gen_bubbles);
            if g.persona_id != persona_id || g.cache.failed.iter().any(|s| s == clip_id) {
                return;
            }
            g.cache.date = crate::genbubble::today_str();
            g.cache.failed.push(clip_id.to_string());
            (g.cache.clone(), true)
        };
        if changed {
            let _ = crate::genbubble::save(&self.app, persona_id, &cache);
        }
    }

    /// 某状态用到的动作是否有当日文案缺口（缓存归属角色不符 / 日期过期 / 有动作既未生成也未标记失败）
    pub(crate) fn gen_needs_refresh(&self, state: &str) -> bool {
        let persona = crate::util::lock(&self.persona);
        let g = crate::util::lock(&self.gen_bubbles);
        if g.persona_id != persona.id {
            return true;
        }
        if g.cache.date != crate::genbubble::today_str() {
            return true;
        }
        let clips = state_clip_ids(&persona, state);
        // 该状态没有任何可播动作时无须生成
        !clips.is_empty()
            && clips.iter().any(|clip| {
                !g.cache.by_clip.contains_key(clip) && !g.cache.failed.iter().any(|f| f == clip)
            })
    }

    /// 设置页开关：写内存 prefs（落盘由调用方 save_prefs 完成）
    pub(crate) fn set_ai_bubbles(&self, enabled: bool) {
        crate::util::lock(&self.prefs).ai_bubbles = enabled;
    }

    // ---- 环境气泡：随机自言自语（与状态切换解耦，机制见 README「主动交互与防打扰机制」）----

    /// 记录一次用户聊天互动（chat_send 入口调用）：随后数分钟内抑制环境气泡
    pub(crate) fn record_chat_activity(&self) {
        crate::util::lock(&self.ambient).last_chat_at = Some(chrono::Local::now());
    }

    /// 用户「注意到角色」（鼠标凑近 / 点一下 / 开口说话）：让角色先放下手上的事看你一眼，
    /// 反应结束再回到原来在做的事。限频避免鼠标蹭过窗口就反复打断。
    pub(crate) fn notify_seen(&self, kind: &str) {
        let now = chrono::Local::now();
        let persona = self.persona();
        let state = self.current_state();
        let Some(steps) = persona
            .acknowledge
            .steps_for(&state, kind)
            .map(<[SceneStepConfig]>::to_vec)
        else {
            return;
        };
        // 被人搭理：社交欲与无聊一起下降（需求层）
        crate::util::lock(&self.needs).on_seen();
        {
            let mut seen = crate::util::lock(&self.seen);
            let pointer = AcknowledgeConfig::is_pointer_kind(kind);
            if pointer
                && seen
                    .last_any
                    .is_some_and(|t| (now - t).num_seconds() < persona.acknowledge.global_cooldown_s)
            {
                return;
            }
            if seen
                .last_kind
                .get(kind)
                .is_some_and(|t| (now - *t).num_seconds() < persona.acknowledge.cooldown_secs(kind))
            {
                return;
            }
            seen.last_any = Some(now);
            seen.last_kind.insert(kind.to_string(), now);
        }
        let event = crate::util::lock(&self.playback).acknowledge(&persona, &state, &steps, now);
        if let Some(event) = event {
            let _ = self.app.emit("playback", event);
            // 反应实例太短不配台词；回到原活动时会重新排说话
            self.reschedule_speech(now);
            self.notify_wake();
        }
    }

    /// 当前状态的期望说话间隔 E（分钟）；mute / 未配置返回 None。
    /// 优先用状态里显式配置的 `bubble_gap_min`，否则按话痨档位取默认值。
    fn state_gap_min(&self, persona: &PersonaConfig, state: &str) -> Option<f64> {
        let cfg = persona.states.get(state);
        let configured = cfg
            .and_then(|c| c.bubble_gap_min)
            .filter(|e| e.is_finite() && *e > 0.0);
        configured
            .or_else(|| default_gap_min(cfg.map(StateConfig::talkativeness).unwrap_or_default()))
    }

    /// 启动 / 切换角色后的第一句话：30~90 秒内，让角色先"活"过来。
    fn arm_first_speech(&self, now: chrono::DateTime<chrono::Local>) {
        let at =
            now + chrono::Duration::seconds(rng_range_i64(FIRST_SPEAK_MIN_S, FIRST_SPEAK_MAX_S));
        crate::util::lock(&self.ambient).earliest_at = Some(at);
    }

    /// 为当前动作实例排定一句话（动作实例开始/推进时调用）。
    ///
    /// 规则：动作实例 ≥30 秒、有可用文案、状态非 mute；说话时刻落在实例的 20%~70% 区间，
    /// 不早于「最早可说话时刻」（静默超过上限时允许提前）；这句话必须能在实例结束前说完。
    /// 台词挂在动作上、且只在动作实例内出现，所以永远不会出现"演 A 说 B"。
    fn reschedule_speech(&self, now: chrono::DateTime<chrono::Local>) {
        let instance = {
            let playback = crate::util::lock(&self.playback);
            playback
                .current()
                .map(|c| {
                    (
                        c.run.started_at,
                        c.run.total_ms,
                        c.run.ack,
                        c.run
                            .beats
                            .iter()
                            .map(|beat| beat.clip.clone())
                            .collect::<Vec<String>>(),
                    )
                })
        };
        let Some((started_at, duration_ms, ack, beat_clips)) = instance else {
            crate::util::lock(&self.ambient).speak_at = None;
            return;
        };
        let persona = self.persona();
        let state = self.current_state();
        let Some(e) = self.state_gap_min(&persona, &state) else {
            crate::util::lock(&self.ambient).speak_at = None;
            return; // mute：当前状态不说话
        };
        // 说话窗口按「段」算：段 ≥30 秒即可说话（段内由若干拍组成，拍本身可以很短）；
        // 「注意到你」这类短反应例外，它就是对用户的即时回应
        if !ack && (duration_ms as i64) < SPEAK_MIN_ACTION_MS {
            crate::util::lock(&self.ambient).speak_at = None;
            return; // 过短的段：不配台词
        }
        // 本段已经排过话、且时刻仍落在本段内 → 保持不动。
        // 否则每换一拍都会重新随机一次，一段可能冒出好几句。
        {
            let ambient = crate::util::lock(&self.ambient);
            if speak_at_in_segment(ambient.speak_at, started_at, duration_ms) {
                return;
            }
        }
        // 整段里没有任何拍有可用文案 → 不排话（避免排了又发不出来的空转）
        let has_text = beat_clips.iter().any(|clip_id| {
            let ai_clip = crate::genbubble::today_pool(&self.gen_bubbles, clip_id);
            resolve_bubble_pool(&persona, Some(clip_id), ai_clip).is_some()
        });
        if !has_text {
            crate::util::lock(&self.ambient).speak_at = None;
            return;
        }
        let mut ambient = crate::util::lock(&self.ambient);
        ambient.speak_at = None;
        let window = if ack {
            ack_speech_window(started_at, duration_ms)
        } else {
            speech_window(
                now,
                started_at,
                duration_ms,
                ambient.earliest_at,
                ambient.last_shown_at,
                e,
            )
        };
        let Some((from, to)) = window else {
            return;
        };
        let span = (to - from).num_seconds().max(0);
        let offset = if span > 0 { rng_range_i64(0, span) } else { 0 };
        ambient.speak_at = Some(from + chrono::Duration::seconds(offset));
    }

    /// 为某状态重开场景播放，并把第一步广播给前端。
    /// 状态切换、切换角色、启动广播都会调用；前端只按事件渲染，不再自己挑场景。
    fn reset_playback(&self, state: &str, now: chrono::DateTime<chrono::Local>) {
        let persona = self.persona();
        self.sync_bias_to_playback();
        let event = crate::util::lock(&self.playback).reset(&persona, state, now);
        if let Some(event) = event {
            self.note_chain_change(&event.chain_id);
            let _ = self.app.emit("playback", event);
        }
        // 新动作实例 → 重新安排这个实例内要不要说话、什么时候说。
        self.reschedule_speech(now);
        // 播放排期变了 → 让节拍线程按新的 next_at 重算睡眠时长。
        // 否则线程可能已经睡到「下一次状态切换」（最长 15 分钟），动作就不会继续往下走。
        self.notify_wake();
    }

    /// 播放推进：到点则进入同场景的下一步，或按权重抽下一个场景
    fn advance_playback(&self, now: chrono::DateTime<chrono::Local>) {
        // 先看是否到点：节拍线程每次唤醒都会调到这里，没到点就别克隆整份 persona
        if !crate::util::lock(&self.playback).is_due(now) {
            return;
        }
        let persona = self.persona();
        self.sync_bias_to_playback();
        let event = crate::util::lock(&self.playback).advance(&persona, now);
        if let Some(event) = event {
            self.note_chain_change(&event.chain_id);
            let _ = self.app.emit("playback", event);
        }
        self.reschedule_speech(now);
    }

    /// 把"需求快照 + 当日计划的标签偏好"一起交给播放层（两者都调制链权重）
    fn sync_bias_to_playback(&self) {
        let needs = crate::util::lock(&self.needs).clone();
        let (focus, avoid) = crate::plan::today_plan(&self.plan)
            .map(|plan| (plan.focus, plan.avoid))
            .unwrap_or_default();
        let mut playback = crate::util::lock(&self.playback);
        playback.set_needs(needs);
        playback.set_plan_tags(focus, avoid);
    }

    /// 今天的计划是否还缺（今天没有、且今天也没失败过）
    pub(crate) fn plan_needs_today(&self) -> bool {
        crate::plan::needs_generation(&self.plan)
    }

    /// 近期主题（跨天）：生成当日计划时要求"换个不一样的"
    pub(crate) fn plan_recent_themes(&self) -> Vec<String> {
        crate::util::lock(&self.plan).cache.recent.clone()
    }

    /// 收下刚生成的当日计划：落盘 + 交给播放层做偏置 + 排进"当天第一次开口"
    pub(crate) fn apply_plan(&self, persona_id: &str, plan: crate::plan::DailyPlan) {
        let cache = {
            let mut p = crate::util::lock(&self.plan);
            if p.persona_id != persona_id {
                return;
            }
            p.cache.date = crate::genbubble::today_str();
            p.cache.failed = false;
            p.cache.recent.push(plan.theme.clone());
            let keep = p
                .cache
                .recent
                .len()
                .saturating_sub(crate::plan::RECENT_KEEP);
            if keep > 0 {
                p.cache.recent.drain(..keep);
            }
            p.cache.plan = Some(plan.clone());
            p.cache.clone()
        };
        let _ = crate::plan::save(&self.app, persona_id, &cache);
        // "要说话"：计划不能只活在权重里，当天第一次开口就把它说出来
        if !plan.say.is_empty() {
            crate::util::lock(&self.ambient).opening = Some(plan.say.clone());
        }
        self.sync_bias_to_playback();
        let _ = self.app.emit("plan-changed", plan);
    }

    /// 记录当天计划生成失败：当天不再重试（跨天后重新试）
    pub(crate) fn record_plan_failure(&self, persona_id: &str) {
        let cache = {
            let mut p = crate::util::lock(&self.plan);
            if p.persona_id != persona_id {
                return;
            }
            p.cache.date = crate::genbubble::today_str();
            p.cache.failed = true;
            p.cache.plan = None;
            p.cache.clone()
        };
        let _ = crate::plan::save(&self.app, persona_id, &cache);
    }

    /// 换了一条新链：新鲜感让"无聊"下降
    fn note_chain_change(&self, chain_id: &str) {
        let mut last = crate::util::lock(&self.last_chain);
        if last.as_deref() != Some(chain_id) {
            *last = Some(chain_id.to_string());
            crate::util::lock(&self.needs).on_new_chain();
        }
    }

    /// 当前正在播放的动作 id（气泡文案按它取池）
    fn current_clip(&self) -> Option<String> {
        crate::util::lock(&self.playback)
            .current_clip()
            .map(str::to_string)
    }

    /// 当前播放快照（动作 id + 中文名 + 画面描述），供对话 system prompt 使用。
    /// 没有正在播放的动作（如前端尚未就绪 / 该状态无有效场景）时返回 None。
    pub fn current_activity(&self) -> Option<ActivityView> {
        let persona = self.persona();
        let (state, scene_id, scene_label, clip_id) = {
            let playback = crate::util::lock(&self.playback);
            let current = playback.current()?;
            (
                current.state.clone(),
                current.run.scene_id.clone(),
                current.run.scene_label.clone(),
                current.clip.clone(),
            )
        };
        let clip = persona.clips.get(&clip_id);
        Some(ActivityView {
            state,
            scene_id,
            scene_label,
            clip: clip_id,
            clip_label: clip.map(|c| c.label.clone()).unwrap_or_default(),
            clip_description: clip.map(|c| c.description.clone()).unwrap_or_default(),
        })
    }

    /// 抑制判断：角色隐藏 / 对话窗打开 / 刚聊过天——命中时气泡推迟而非取消。
    /// UI 可见性查询不做锁内操作，仅在锁外调用。
    fn bubble_suppressed(&self, now: &chrono::DateTime<chrono::Local>) -> bool {
        if let Some(win) = self.app.get_webview_window("persona") {
            if !win.is_visible().unwrap_or(false) {
                return true;
            }
        }
        if let Some(chat) = self.app.get_webview_window("chat") {
            if chat.is_visible().unwrap_or(false) {
                return true;
            }
        }
        if let Some(t) = crate::util::lock(&self.ambient).last_chat_at {
            if *now - t < chrono::Duration::minutes(CHAT_SUPPRESS_MIN) {
                return true;
            }
        }
        false
    }

    /// 取当前动作的下一条气泡文案（池来源与优先级见 [`resolve_bubble_pool`]）。
    fn next_ambient_text(&self) -> Option<String> {
        // 当日计划的"开场白"优先：当天第一次开口就说「今天想怎么过」，说完即清
        if let Some(say) = crate::util::lock(&self.ambient).opening.take() {
            return Some(say);
        }
        let persona = self.persona();
        let clip_id = self.current_clip();
        let ai_clip = clip_id
            .as_deref()
            .and_then(|id| crate::genbubble::today_pool(&self.gen_bubbles, id));
        let (key, pool) = resolve_bubble_pool(&persona, clip_id.as_deref(), ai_clip)?;
        self.draw_from_bag(&key, || pool)
    }

    /// 从指定池键的洗牌袋取一条；袋空时用 `build_pool` 重装（避免无谓地反复取大池）
    fn draw_from_bag(&self, key: &str, build_pool: impl FnOnce() -> Vec<String>) -> Option<String> {
        {
            let mut ambient = crate::util::lock(&self.ambient);
            if let Some(bag) = ambient.bags.get_mut(key) {
                if let Some(text) = bag.pop_front() {
                    ambient.last_text.insert(key.to_string(), text.clone());
                    return Some(text);
                }
            }
        }
        let pool = build_pool();
        if pool.is_empty() {
            return None;
        }
        let mut ambient = crate::util::lock(&self.ambient);
        let mut bag = refill_bag(pool, ambient.last_text.get(key).map(String::as_str));
        let text = bag.pop_front()?;
        ambient.bags.insert(key.to_string(), bag);
        ambient.last_text.insert(key.to_string(), text.clone());
        Some(text)
    }

    /// 到点则说一句：处于抑制窗口就放弃这个动作实例（等下一个），
    /// 否则取当前动作的文案，并附带"能在实例结束前说完"的展示时长。
    fn fire_ambient_bubble_if_due(&self, now: chrono::DateTime<chrono::Local>) {
        let due = crate::util::lock(&self.ambient)
            .speak_at
            .is_some_and(|t| now >= t);
        if !due {
            return;
        }
        crate::util::lock(&self.ambient).speak_at = None;
        if self.bubble_suppressed(&now) {
            return;
        }
        let remaining_ms = crate::util::lock(&self.playback)
            .remaining_ms(now)
            .unwrap_or(BUBBLE_DEFAULT_SHOW_MS);
        let show_ms = BUBBLE_DEFAULT_SHOW_MS.min(remaining_ms.max(0)) as u64;
        if show_ms == 0 {
            return;
        }
        let Some(text) = self.next_ambient_text() else {
            return;
        };
        let persona = self.persona();
        let state = self.current_state();
        let next_earliest = self
            .state_gap_min(&persona, &state)
            .map(|e| now + chrono::Duration::seconds((sample_gap_min(e) * 60.0) as i64));
        {
            let mut ambient = crate::util::lock(&self.ambient);
            ambient.last_shown_at = Some(now);
            ambient.earliest_at = next_earliest;
        }
        let _ = self.app.emit("bubble", BubbleEvent { text, show_ms });
    }

    /// 应用全局缩放：clamp 校验（非 finite → 1.0，越界 → [0.5, 2.0]），更新内存 prefs 并刷新
    /// display_size 缓存。返回是否真的发生变化（与当前相同则直接返回 false，避免无谓写盘/重排）。
    pub fn apply_zoom(&self, zoom: f64) -> bool {
        let zoom = if zoom.is_finite() {
            zoom.clamp(crate::prefs::MIN_ZOOM, crate::prefs::MAX_ZOOM)
        } else {
            crate::prefs::default_zoom()
        };
        {
            let mut p = crate::util::lock(&self.prefs);
            if p.zoom == zoom {
                return false;
            }
            p.zoom = zoom;
        }
        self.refresh_display_size();
        true
    }

    /// 重新计算当前角色的缩放后显示尺寸并写回缓存。各个锁按序获取、互不嵌套，
    /// 符合项目“锁内不做耗时操作、先算后写”的约定，避免锁序问题。
    pub fn refresh_display_size(&self) {
        let zoom = crate::util::lock(&self.prefs).zoom;
        let persona = self.persona();
        let mut display = effective_display_size_zoomed(&persona, zoom);
        // 此刻未持有任何锁：取工作区（UI 查询不做锁内操作），按工作区等比适配后写回缓存。
        if let Some((mw, mh)) = work_area_content_limit(&self.app) {
            display = fit_display_size_proportional(display.0, display.1, mw, mh);
        }
        *crate::util::lock(&self.display_size) = display;
    }

    /// 返回给前端的角色视图：只带渲染需要的字段（缩放后的显示尺寸 + 状态名）。
    /// 动作的渲染参数由 `playback` 事件自带，clips/scenes 不再推给前端，省掉每次广播的
    /// 大段序列化与解析。
    pub fn persona_view(&self) -> PersonaView {
        let persona = self.persona();
        let (w, h) = self.display_size();
        PersonaView {
            id: persona.id.clone(),
            name: persona.name.clone(),
            display_w: w,
            display_h: h,
            states: persona.states.clone(),
        }
    }

    /// 唤醒后台节拍线程：switch_persona 在换角色后调用，
    /// 让它在新的日程下重新计算下一次切换时刻，避免睡到旧的 next_transition_at。
    /// 只锁 wake_lock（不持有 persona），与线程侧锁序保持一致，避免死锁。
    fn notify_wake(&self) {
        let _guard = crate::util::lock(&self.wake_lock);
        self.wake_cond.notify_one();
    }

    /// 用户导入角色的持久化目录（%APPDATA%\com.deskzen.desktop\characters\）
    pub fn characters_dir(&self) -> Result<PathBuf, String> {
        Self::characters_dir_for(&self.app)
    }

    fn characters_dir_for(app: &AppHandle) -> Result<PathBuf, String> {
        let dir = app
            .path()
            .app_config_dir()
            .map_err(|e| format!("无法定位应用数据目录: {e}"))?;
        Ok(dir.join("characters"))
    }

    /// 运行时注册一个角色（本地导入后调用；已存在则覆盖）
    pub fn register_persona(&self, cfg: PersonaConfig) {
        crate::util::lock(&self.personas).insert(cfg.id.clone(), Arc::new(cfg));
    }

    /// 删除导入角色（local-*）：先删除磁盘目录，再从注册表移除。
    /// 内置角色（如 link）不允许删除。返回被删除角色的显示名。
    pub fn remove_persona(&self, id: &str) -> Result<String, String> {
        if !id.starts_with("local-") {
            return Err("内置角色不可删除".into());
        }
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("角色 id 不合法".into());
        }
        let dir = self.characters_dir()?.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("删除角色文件失败: {e}"))?;
        }
        // 一并删除该角色的持久化对话历史（存在才删；删除失败不阻断主流程——避免“删角色后
        // 重新导入同名角色想起上一世对话”的意外；极端下文件残留也不影响使用）。
        if let Ok(conf) = self.app.path().app_config_dir() {
            let history_file = conf.join("history").join(format!("{id}.json"));
            if history_file.exists() {
                let _ = std::fs::remove_file(&history_file);
            }
        }
        let name = crate::util::lock(&self.personas)
            .remove(id)
            .map(|p| p.name.clone())
            .ok_or_else(|| format!("角色不存在: {id}"))?;
        Ok(name)
    }

    /// 所有可切换角色 (id, 显示名)
    pub fn list_personas(&self) -> Vec<(String, String)> {
        let mut sorted: Vec<(String, String)> = crate::util::lock(&self.personas)
            .iter()
            .map(|(id, p)| (id.clone(), p.name.clone()))
            .collect();
        // 直接遍历 HashMap 会让托盘“更换角色”菜单每次启动顺序随机；这里按
        // 「内置角色在前（按内置列表固定顺序），导入角色按名称字典序」排序，让菜单稳定且更易找。
        let embedded: Vec<&str> = EMBEDDED_PERSONAS.iter().map(|(id, _)| *id).collect();
        sorted.sort_by(|a, b| {
            let rank = |id: &str| embedded.iter().position(|e| *e == id);
            match (rank(&a.0), rank(&b.0)) {
                (Some(ai), Some(bi)) => ai.cmp(&bi),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.1.cmp(&b.1),
            }
        });
        sorted
    }

    /// 运行时切换角色：更新配置与作息表，并广播事件让前端重新渲染
    pub fn switch_persona(&self, app: &AppHandle, id: &str) -> Result<(), String> {
        // 先记下"切换前的角色 id"：下面会覆盖 self.persona，晚了就读到新角色了
        let previous_id = crate::util::lock(&self.persona).id.clone();
        let persona = crate::util::lock(&self.personas)
            .get(id)
            .cloned()
            .ok_or_else(|| format!("角色不存在: {id}"))?;
        let zoom = crate::util::lock(&self.prefs).zoom;
        let mut display = effective_display_size_zoomed(&persona, zoom);
        // 切换后同样按工作区等比适配，避免新角色尺寸过大出屏。
        if let Some((mw, mh)) = work_area_content_limit(&self.app) {
            display = fit_display_size_proportional(display.0, display.1, mw, mh);
        }
        // 新角色的当前状态：切换角色本身不算“状态变化”，预先记录 last_state，
        // 避免节拍线程醒来误判为状态切换而重复广播（气泡重排由下方显式完成）。
        let state = self.resolve_state(&persona, chrono::Local::now());
        {
            let mut cur = crate::util::lock(&self.persona);
            *cur = Arc::clone(&persona);
            *crate::util::lock(&self.last_state) = Some(state.clone());
            *crate::util::lock(&self.display_size) = display;
        }
        // 需求/情绪按角色独立：先把旧角色的存档写回，再载入新角色（各自有各自的体力）
        {
            let snapshot = crate::util::lock(&self.needs).clone();
            let _ = crate::needs::save(&self.app, &previous_id, &snapshot);
            *crate::util::lock(&self.needs) = crate::needs::load(&self.app, id, &persona.needs);
        }
        *crate::util::lock(&self.last_chain) = None;
        // 当日计划同样按角色独立：换成本角色今天的计划（已经生成过的直接复用）
        {
            let cache = crate::plan::load(&self.app, id);
            *crate::util::lock(&self.plan) = crate::plan::PlanState {
                persona_id: id.to_string(),
                cache,
            };
        }
        // 上一位角色的"开场白"不能留给新角色说
        crate::util::lock(&self.ambient).opening = None;
        // 气泡缓存整体切到新角色（内存换绑 + 磁盘加载），并视条件补跑当日生成
        {
            let cache = crate::genbubble::load(&self.app, id);
            *crate::util::lock(&self.gen_bubbles) = crate::genbubble::GenState {
                persona_id: id.to_string(),
                cache,
            };
        }
        // 洗牌袋与“上一条文案”属于旧角色，整体清空；last_shown_at / last_chat_at 保留——
        // 避免借连续切角色重置静默上限与聊天抑制窗口。
        {
            let mut ambient = crate::util::lock(&self.ambient);
            ambient.bags.clear();
            ambient.last_text.clear();
            ambient.speak_at = None;
        }
        // 场景洗牌袋与"当前播放"都属于旧角色，整体清空后按新角色重开
        crate::util::lock(&self.playback).clear();
        // 切换角色后 30~90 秒内先说一句，别让人以为坏了（reset_playback 会据此排期）
        let now = chrono::Local::now();
        self.arm_first_speech(now);
        crate::genbubble::maybe_spawn_for_state(app);
        crate::plan::maybe_spawn_for_today(app);
        // 修改完成后唤醒节拍线程，使新日程表立即生效（否则线程仍睡在旧 next_transition_at）。
        self.notify_wake();
        // 给前端的显示配置带 zoomed 尺寸，前端 applyPersona 据此零改动呈现缩放后的角色。
        let _ = app.emit("persona-changed", self.persona_view());
        let _ = app.emit(
            "state-changed",
            StateChanged {
                state: state.clone(),
            },
        );
        self.reset_playback(&state, now);
        // 新角色缩放后尺寸可能不同：同步重设窗口并保持地板不动。
        crate::resize_persona_window(&self.app);
        Ok(())
    }

    /// 启动即广播当前角色与状态，并排出第一条环境气泡（启动时不立即弹出）
    pub fn broadcast_state(&self) {
        let persona = self.persona();
        let now = chrono::Local::now();
        let state = self.resolve_state(&persona, now);
        // 记录“该状态已广播过”，否则节拍线程醒来读到 last_state=None 会误判为状态变化，
        // 再次 emit state-changed，导致开局重复广播。
        *crate::util::lock(&self.last_state) = Some(state.clone());
        // 带 zoomed display 给前端，前端 applyPersona 不因广播而回到基准尺寸。
        let _ = self.app.emit("persona-changed", self.persona_view());
        let _ = self.app.emit(
            "state-changed",
            StateChanged {
                state: state.clone(),
            },
        );
        // 启动后的第一句话排在 30~90 秒后（不立即弹，给角色一段安静期）
        self.arm_first_speech(now);
        // 前端拿到角色配置后再给播放指令，顺序保证前端能解析到动作资源
        self.reset_playback(&state, now);
    }

    /// 后台节拍线程：睡到「下一个状态切换时刻」与「下一条环境气泡时刻」中较早者，
    /// 醒来先处理状态变化（广播 + 重排气泡），再处理到期气泡；单次睡眠最多 15 分钟。
    pub fn start(&self) {
        let app = self.app.clone();
        let persona = Arc::clone(&self.persona);
        let ambient = Arc::clone(&self.ambient);
        let playback = Arc::clone(&self.playback);
        let last_state = Arc::clone(&self.last_state);
        let wake_lock = Arc::clone(&self.wake_lock);
        let wake_cond = Arc::clone(&self.wake_cond);
        let needs = Arc::clone(&self.needs);
        let mut needs_saved_at = chrono::Local::now().timestamp();
        // 当日计划的"今天"标记：跨天时补一次生成，同一天不反复试
        let mut plan_checked_day = crate::genbubble::today_str();
        thread::spawn(move || loop {
            // 先算出下一次状态切换时刻，再用带超时的 Condvar 等待（+1s 缓冲，避免边界竞态）。
            // 单次睡眠不超过 15 分钟：防止时钟漂移 / DST 导致久睡不醒，醒来重算即可。
            // 持有 wake_lock 计算并进入 wait：switch_persona 的 notify
            // 必然在本线程进入等待后送达，不会丢失唤醒（唤醒后统一走下面的重算）。
            let now = chrono::Local::now();
            let wake = crate::util::lock(&wake_lock);
            let next = {
                let p = crate::util::lock(&persona);
                next_transition_at(&p, &now)
            };
            // 当前动作实例排定的说话时刻若早于状态切换，则按说话时刻唤醒
            let (wake_at, needs_precise_wake) = {
                let bubble_at = crate::util::lock(&ambient).speak_at;
                let play_at = crate::util::lock(&playback).next_at();
                let mut earliest = next;
                if let Some(t) = bubble_at {
                    earliest = earliest.min(t);
                }
                if let Some(t) = play_at {
                    earliest = earliest.min(t);
                }
                // 唤醒来自「该说话了」或「动作播完了」时要踩点：这两件事都是秒级接力，
                // 1 秒防抖会把短反应（如「注意到你」）的台词挤到临结束才出现。
                let precise = bubble_at.is_some_and(|t| t == earliest)
                    || play_at.is_some_and(|t| t == earliest);
                (earliest, precise)
            };
            let mut sleep_dur = (wake_at - now).to_std().unwrap_or(Duration::from_secs(0));
            // 状态切换按分钟对齐，多睡 1 秒可避开边界竞态；说话/动作推进都是秒级接力，
            // 多睡 1 秒会让画面在切换前"定格一下"，所以只留 60ms 余量。
            sleep_dur += if needs_precise_wake {
                Duration::from_millis(60)
            } else {
                Duration::from_secs(1)
            };
            let cap = Duration::from_secs(15 * 60);
            if sleep_dur > cap {
                sleep_dur = cap;
            }
            // 超时或被 notify 唤醒都返回；释放锁后继续下面的状态检测。
            let (guard, _) = wake_cond.wait_timeout(wake, sleep_dur).unwrap();
            drop(guard);

            let now = chrono::Local::now();
            // 先按"上一次所处状态"推进需求（tick 间隔可能是几秒，也可能是 15 分钟）
            let needs_now = {
                let prev_state = crate::util::lock(&last_state)
                    .clone()
                    .unwrap_or_else(|| FALLBACK_STATE.to_string());
                // 锁序：persona → needs（与其它路径一致）
                let p = crate::util::lock(&persona);
                let mut n = crate::util::lock(&needs);
                let seconds = if n.updated_unix > 0 {
                    (now.timestamp() - n.updated_unix).clamp(0, 3600)
                } else {
                    0
                };
                n.advance(&prev_state, seconds as f64 / 3600.0, &p.needs);
                n.updated_unix = now.timestamp();
                n.clone()
            };
            let state = {
                let p = crate::util::lock(&persona);
                runtime_state(&p, &automatic_state(&p, now_minutes(&now), &needs_now))
            };
            // 需求状态每 5 分钟落一次盘（保证重启后有连续感，又不至于频繁写盘）
            if now.timestamp() - needs_saved_at >= 300 {
                needs_saved_at = now.timestamp();
                let snapshot = crate::util::lock(&needs).clone();
                let persona_id = crate::util::lock(&persona).id.clone();
                let _ = crate::needs::save(&app, &persona_id, &snapshot);
            }
            let mut last = crate::util::lock(&last_state);
            let mut state_changed = false;
            if last.as_deref() != Some(state.as_str()) {
                *last = Some(state.clone());
                let _ = app.emit(
                    "state-changed",
                    StateChanged {
                        state: state.clone(),
                    },
                );
                state_changed = true;
            }
            drop(last);
            let engine = app.state::<StateEngine>();
            // 跨天补一次当日计划（生成失败当天不再重试，见 plan::maybe_spawn_for_today）
            let today = crate::genbubble::today_str();
            if plan_checked_day != today {
                plan_checked_day = today;
                crate::plan::maybe_spawn_for_today(&app);
            }
            if state_changed {
                // 新状态换一套编排：重开播放并把第一步广播给前端；
                // 说话排期随新的动作实例重排（状态切换本身不弹气泡）。
                engine.reset_playback(&state, now);
                // 跨天/换状态时才需要补齐当日 AI 气泡：放在这里避免每个动作（几秒一次）
                // 都去锁 persona + 缓存做一遍无谓检查
                crate::genbubble::maybe_spawn_for_state(&app);
            }
            // 到点就推进场景步骤（同场景的下一步 / 下一个场景）
            engine.advance_playback(now);
            // 处理到期气泡：若刚重排过，next_at 在未来，自然跳过，不会用旧状态文案。
            engine.fire_ambient_bubble_if_due(now);
        });
    }

}

#[tauri::command]
pub fn get_persona_config(engine: tauri::State<'_, StateEngine>) -> PersonaView {
    // 返回带缩放后 display_w/h 的视图：前端 applyPersona 据此直接呈现缩放后的角色。
    engine.persona_view()
}

#[tauri::command]
pub fn get_current_state(engine: tauri::State<'_, StateEngine>) -> String {
    engine.current_state()
}

/// 前端上报「用户注意到角色」：kind = hover | click | talk
#[tauri::command]
pub fn notify_seen(kind: String, engine: tauri::State<'_, StateEngine>) {
    engine.notify_seen(&kind);
}

/// 对话历史文件最大加载字节数：超过则放弃加载（防手工塞入巨型文件拖慢启动）
const HISTORY_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// 持久化保留的最近消息条数（与前端裁剪一致，双保险）
const HISTORY_MAX_MESSAGES: usize = 20;

/// 角色 id 合法性：仅字母数字与连字符（防路径穿越；与 remove_persona 校验一致）
pub(crate) fn is_valid_history_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// 历史角色白名单：只保留 user/assistant，其余（如 system）丢弃
fn is_valid_history_role(role: &str) -> bool {
    role == "user" || role == "assistant"
}

/// 规范化历史：先过滤无用角色，再保留最近 20 条（与前端裁剪一致，双保险）。
fn sanitize_history(messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
    let mut kept: Vec<LlmMessage> = messages
        .into_iter()
        .filter(|m| is_valid_history_role(&m.role))
        .collect();
    if kept.len() > HISTORY_MAX_MESSAGES {
        kept = kept.split_off(kept.len() - HISTORY_MAX_MESSAGES);
    }
    kept
}

/// 历史文件路径：%APPDATA%\com.deskzen.desktop\history\{persona_id}.json
fn history_file_path(app: &AppHandle, persona_id: &str) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("history").join(format!("{persona_id}.json")))
}

/// 对话历史记录格式（含版本号，便于将来扩展）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryRecord {
    #[serde(default = "default_history_version")]
    version: u32,
    messages: Vec<LlmMessage>,
}

fn default_history_version() -> u32 {
    1
}

/// 加载某角色的持久化对话历史；任何失败（目录/文件不存在、解析失败、文件过大）都返回空 Vec，
/// 不报错——历史缺失不应打断对话。
#[tauri::command]
pub fn load_chat_history(app: AppHandle, persona_id: String) -> Vec<LlmMessage> {
    if !is_valid_history_id(&persona_id) {
        return Vec::new();
    }
    let Ok(path) = history_file_path(&app, &persona_id) else {
        return Vec::new();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return Vec::new();
    };
    if meta.len() > HISTORY_MAX_BYTES {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(record) = serde_json::from_str::<HistoryRecord>(&content) else {
        return Vec::new();
    };
    record
        .messages
        .into_iter()
        .filter(|m| is_valid_history_role(&m.role))
        .collect()
}

/// 保存某角色的持久化对话历史：id 校验、消息规范化（角色白名单 + 最近 20 条）、临时文件 + rename 原子写。
#[tauri::command]
pub fn save_chat_history(
    app: AppHandle,
    persona_id: String,
    messages: Vec<LlmMessage>,
) -> Result<(), String> {
    if !is_valid_history_id(&persona_id) {
        return Err("角色 id 不合法".into());
    }
    let messages = sanitize_history(messages);
    let record = HistoryRecord {
        version: 1,
        messages,
    };
    let path = history_file_path(&app, &persona_id)?;
    let json = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    crate::util::atomic_write(&path, &json)
}

fn now_minutes(now: &chrono::DateTime<chrono::Local>) -> u32 {
    now.hour() * 60 + now.minute()
}

/// 查找当前生效的 time 时段（支持跨午夜）
fn find_active_slot(schedule: &ScheduleConfig, mins: u32) -> Option<&TimeSlot> {
    schedule.time().iter().find(|slot| {
        if let (Some(start), Some(end)) = (parse_mins(&slot.start), parse_mins(&slot.end)) {
            if start <= end {
                mins >= start && mins < end
            } else {
                // 跨午夜段：00:00~end 归入前一段
                mins >= start || mins < end
            }
        } else {
            false
        }
    })
}

/// 「注意到你」这类短反应的说话窗口。
/// 已排定的说话时刻是否仍落在这一段内（用于「一段最多一句」：段内换拍时不重新随机）
fn speak_at_in_segment(
    speak_at: Option<chrono::DateTime<chrono::Local>>,
    segment_started_at: chrono::DateTime<chrono::Local>,
    segment_ms: u64,
) -> bool {
    speak_at.is_some_and(|t| {
        t >= segment_started_at
            && t <= segment_started_at + chrono::Duration::milliseconds(segment_ms as i64)
    })
}

///
/// 反应通常只有 4~6 秒，用常规规则（≥30 秒才说话）一句都说不了。这里放宽为：
/// 反应 ≥2.5 秒即可说话，时刻落在反应的 25% 处，展示到反应结束前 1.5 秒为止
/// （气泡展示时长本来就会被实例剩余时间截短）；间隔护栏不适用——这是对用户的即时回应。
fn ack_speech_window(
    started_at: chrono::DateTime<chrono::Local>,
    duration_ms: u64,
) -> Option<(
    chrono::DateTime<chrono::Local>,
    chrono::DateTime<chrono::Local>,
)> {
    const ACK_SPEAK_MIN_MS: i64 = 2_500;
    const ACK_LINE_MIN_MS: i64 = 1_500;
    let duration = duration_ms as i64;
    if duration < ACK_SPEAK_MIN_MS {
        return None;
    }
    let ends_at = started_at + chrono::Duration::milliseconds(duration);
    let from = started_at + chrono::Duration::milliseconds(duration / 4);
    let to = ends_at - chrono::Duration::milliseconds(ACK_LINE_MIN_MS);
    Some((from, if to > from { to } else { from }))
}

/// 依据墙钟时间在循环状态中取当前状态
fn loop_state_at(schedule: &ScheduleConfig, mins: u32) -> Option<String> {
    let entries = schedule.loop_entries();
    if entries.is_empty() {
        return None;
    }
    let total: u64 = entries.iter().map(|e| e.duration as u64).sum();
    if total == 0 {
        return None;
    }
    let mut idx = (mins as u64) % total;
    for e in entries {
        if idx < e.duration as u64 {
            return Some(e.state.clone());
        }
        idx -= e.duration as u64;
    }
    None
}

/// 状态遍历顺序：优先按 schedule.loop 的出场顺序（去重），其余状态按字典序排在后面。
/// clips/scenes 模型不再有 spritesheet 行号，行为顺序改由日程定义，避免 HashMap 无序遍历。
pub(crate) fn ordered_state_keys(persona: &PersonaConfig) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for entry in persona.schedule.loop_entries() {
        if persona.states.contains_key(&entry.state) && !keys.contains(&entry.state) {
            keys.push(entry.state.clone());
        }
    }
    let mut rest: Vec<String> = persona
        .states
        .keys()
        .filter(|key| !keys.contains(key))
        .cloned()
        .collect();
    rest.sort();
    keys.extend(rest);
    keys
}

/// 单个节拍的参数校验（段与「注意到你」反应共用）
fn validate_step(step: &SceneStepConfig, owner: &str) -> Result<(), String> {
    match &step.seconds {
        Some(SecondsSpec::Fixed(0)) => {
            return Err(format!("{owner} 的动作 {} seconds 必须大于 0", step.clip));
        }
        Some(SecondsSpec::Range([min, max])) if *min == 0 || *max == 0 || min > max => {
            return Err(format!(
                "{owner} 的动作 {} seconds 区间非法（需 1 ≤ min ≤ max，实际 [{min}, {max}]）",
                step.clip
            ));
        }
        _ => {}
    }
    if !(0.0..=1.0).contains(&step.chance) || !step.chance.is_finite() {
        return Err(format!(
            "{owner} 的动作 {} chance 必须在 0~1 之间",
            step.clip
        ));
    }
    Ok(())
}

/// 角色配置的**结构**校验（不含资源文件）：导入与启动扫描共用同一套判据。
///
/// 之前导入侧校验很严、加载侧只做粗略准入，结果是"手改过或旧版本写的"角色包能被注册，
/// 但某些状态引用的动作不存在，运行时被静默过滤——那个状态就什么都不播，前端停在上一个画面。
pub(crate) fn validate_persona_structure(persona: &PersonaConfig) -> Result<(), String> {
    if persona.states.is_empty() {
        return Err("缺少 states 状态定义".into());
    }
    if !persona.states.contains_key(FALLBACK_STATE) {
        return Err(format!("缺少必配状态 {FALLBACK_STATE}（日常）"));
    }
    for state in persona.states.keys() {
        if !CANONICAL_STATES.contains(&state.as_str()) {
            return Err(format!(
                "状态 {state} 不在固定状态集合（routine/focus/active/relax/eat/sleep）内"
            ));
        }
    }
    if persona.clips.is_empty() {
        return Err("缺少 clips 动作定义".into());
    }
    for (clip_id, clip) in &persona.clips {
        if clip.frames == 0 || clip.frame_ms == 0 {
            return Err(format!("动作 {clip_id} 的 frames 与 frame_ms 必须大于 0"));
        }
    }
    // 活动校验：每个状态至少一条链，链里至少一个段，段里至少一拍，引用的动作必须已定义。
    // 调用方应先 [`PersonaConfig::finalize`]（段 id / label 已补齐），否则查不到重复 id。
    for (state, cfg) in &persona.states {
        if cfg.chains.is_empty() {
            return Err(format!("状态 {state} 没有可播放的活动（chains）"));
        }
        let mut chain_ids: Vec<&str> = Vec::new();
        let mut segment_ids: Vec<&str> = Vec::new();
        for chain in &cfg.chains {
            if chain_ids.contains(&chain.id.as_str()) {
                return Err(format!("状态 {state} 里有重名的链 {}", chain.id));
            }
            chain_ids.push(&chain.id);
            if !chain.steps.is_empty() {
                return Err(format!(
                    "链 {} 同时写了 segments 与 steps（单段链只能二选一）",
                    chain.id
                ));
            }
            if chain.segments.is_empty() {
                return Err(format!("链 {} 没有段", chain.id));
            }
            for segment in &chain.segments {
                if segment.steps.is_empty() {
                    return Err(format!("段 {} 没有动作步骤", segment.id));
                }
                if segment_ids.contains(&segment.id.as_str()) {
                    return Err(format!("状态 {state} 里有重名的段 {}", segment.id));
                }
                segment_ids.push(&segment.id);
                for step in &segment.steps {
                    if !persona.clips.contains_key(&step.clip) {
                        return Err(format!("段 {} 引用了未知动作 {}", segment.id, step.clip));
                    }
                    validate_step(step, &format!("段 {}", segment.id))?;
                }
            }
            if let Some(when) = &chain.when {
                for clip in &when.requires_recent {
                    if !persona.clips.contains_key(clip) {
                        return Err(format!(
                            "链 {} 的 when.requires_recent 引用了未知动作 {clip}",
                            chain.id
                        ));
                    }
                }
                if when.max_per_day == Some(0) {
                    return Err(format!("链 {} 的 when.max_per_day 必须大于 0", chain.id));
                }
            }
        }
    }
    // 日程引用校验：状态必须来自固定集合。引用"未配置素材"的状态是允许的——
    // 运行时该状态完全按 routine 处理（见 runtime_state），不算错误。
    // 「被注意到」的即时反应：来源/状态键合法、步骤引用的动作必须存在、限频参数合理
    let ack = &persona.acknowledge;
    for kind in ack.by_kind.keys() {
        if !ACK_KINDS.contains(&kind.as_str()) {
            return Err(format!("acknowledge.by_kind 含未知来源 {kind}"));
        }
    }
    for state in ack.by_state.keys() {
        if !CANONICAL_STATES.contains(&state.as_str()) {
            return Err(format!("acknowledge.by_state 含未知状态 {state}"));
        }
    }
    for kind in ack.cooldown_s.keys() {
        if kind != DEFAULT_ACK_KEY && !ACK_KINDS.contains(&kind.as_str()) {
            return Err(format!("acknowledge.cooldown_s 含未知来源 {kind}"));
        }
    }
    for (label, steps) in ack
        .by_kind
        .iter()
        .map(|(kind, steps)| (format!("by_kind[{kind}]"), steps))
        .chain(
            ack.by_state
                .iter()
                .map(|(state, steps)| (format!("by_state[{state}]"), steps)),
        )
        .chain(std::iter::once(("default".to_string(), &ack.default)))
    {
        for step in steps {
            if !persona.clips.contains_key(&step.clip) {
                return Err(format!("acknowledge.{label} 引用了未知动作 {}", step.clip));
            }
            validate_step(step, &format!("acknowledge.{label}"))?;
        }
    }
    if ack.cooldown_s.values().any(|secs| *secs < 0)
        || ack.global_cooldown_s < 0
        || ack.min_remaining_s < 0
    {
        return Err("acknowledge 的间隔参数不能为负".into());
    }
    // 需求/情绪配置：键必须是已知需求、状态必须是固定集合、阈值在 0~1
    let needs = &persona.needs;
    for (need, value) in &needs.start {
        if !crate::needs::NEED_NAMES.contains(&need.as_str()) {
            return Err(format!("needs.start 含未知需求 {need}"));
        }
        if !(0.0..=1.0).contains(value) {
            return Err(format!("needs.start.{need} 必须在 0~1 之间"));
        }
    }
    for (need, rate) in &needs.rates {
        if !crate::needs::NEED_NAMES.contains(&need.as_str()) {
            return Err(format!("needs.rates 含未知需求 {need}"));
        }
        if !rate.is_finite() {
            return Err(format!("needs.rates.{need} 必须是有限数值"));
        }
    }
    for (need, by_state) in &needs.restore {
        if !crate::needs::NEED_NAMES.contains(&need.as_str()) {
            return Err(format!("needs.restore 含未知需求 {need}"));
        }
        for (state, rate) in by_state {
            if !CANONICAL_STATES.contains(&state.as_str()) {
                return Err(format!("needs.restore.{need} 含未知状态 {state}"));
            }
            if !rate.is_finite() {
                return Err(format!("needs.restore.{need}.{state} 必须是有限数值"));
            }
        }
    }
    for rule in &needs.pull {
        if !crate::needs::NEED_NAMES.contains(&rule.need.as_str()) {
            return Err(format!("needs.pull 含未知需求 {}", rule.need));
        }
        if !CANONICAL_STATES.contains(&rule.state.as_str()) {
            return Err(format!("needs.pull 拉向未知状态 {}", rule.state));
        }
        if rule.above.is_none() && rule.below.is_none() {
            return Err(format!(
                "needs.pull({} → {}) 至少要写 above 或 below",
                rule.need, rule.state
            ));
        }
        for threshold in [rule.above, rule.below].into_iter().flatten() {
            if !(0.0..=1.0).contains(&threshold) {
                return Err(format!(
                    "needs.pull({} → {}) 的阈值必须在 0~1 之间",
                    rule.need, rule.state
                ));
            }
        }
    }
    for rule in &needs.chain_bias {
        if !crate::needs::NEED_NAMES.contains(&rule.need.as_str()) {
            return Err(format!("needs.chain_bias 含未知需求 {}", rule.need));
        }
        if rule.tag.trim().is_empty() {
            return Err("needs.chain_bias 的 tag 不能为空".into());
        }
        if !rule.factor.is_finite() || rule.factor < 0.0 {
            return Err(format!(
                "needs.chain_bias({} × {}) 的 factor 必须是非负数",
                rule.need, rule.tag
            ));
        }
        if rule.above.is_none() && rule.below.is_none() {
            return Err(format!(
                "needs.chain_bias({} × {}) 至少要写 above 或 below",
                rule.need, rule.tag
            ));
        }
    }
    for entry in persona.schedule.loop_entries() {
        if !CANONICAL_STATES.contains(&entry.state.as_str()) {
            return Err(format!("循环引用了未知状态 {}", entry.state));
        }
        if entry.duration == 0 {
            return Err(format!("循环状态 {} 的 duration 必须大于 0", entry.state));
        }
    }
    for slot in persona.schedule.time() {
        if !CANONICAL_STATES.contains(&slot.state.as_str()) {
            return Err(format!("time 时段引用了未知状态 {}", slot.state));
        }
        if parse_mins(&slot.start).is_none() || parse_mins(&slot.end).is_none() {
            return Err(format!(
                "time 时段 {}-{} 的时间格式非法（应为 HH:MM）",
                slot.start, slot.end
            ));
        }
    }
    Ok(())
}

/// 状态是否有可播放素材（配置了至少一个非空段）。
pub(crate) fn state_has_material(persona: &PersonaConfig, state: &str) -> bool {
    persona
        .states
        .get(state)
        .is_some_and(|cfg| cfg.chains.iter().any(|chain| !chain.segments.is_empty()))
}

/// 运行时有效状态：角色没配置该状态（无素材）时，完全按 routine 处理
/// （画面、语气、气泡频率、免打扰都不做特殊处理）。
pub(crate) fn runtime_state(persona: &PersonaConfig, state: &str) -> String {
    if state == FALLBACK_STATE || state_has_material(persona, state) {
        state.to_string()
    } else {
        FALLBACK_STATE.to_string()
    }
}

/// 某状态会用到哪些动作（按链/段顺序去重）——AI 每日文案按动作粒度生成，
/// 只需要生成当前状态可能播到的动作，控制每日调用量。
pub(crate) fn state_clip_ids(persona: &PersonaConfig, state: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let Some(cfg) = persona.states.get(state) else {
        return ids;
    };
    for chain in &cfg.chains {
        for segment in &chain.segments {
            for step in &segment.steps {
                if persona.clips.contains_key(&step.clip) && !ids.contains(&step.clip) {
                    ids.push(step.clip.clone());
                }
            }
        }
    }
    ids
}

/// 决定这次气泡用哪个文案池，返回（洗牌袋键, 文案池）。
/// 文案只挂在动作上，保证"说的话"必然对应"正在演的动作"。
/// 当日 AI 台词与动作自带台词**合并**成一个池（AI 在前、去重）：
/// AI 偶尔只产出 1~2 条，合并后池子才不会薄到反复念同一句；没有动作或池空则不弹。
pub(crate) fn resolve_bubble_pool(
    persona: &PersonaConfig,
    clip_id: Option<&str>,
    ai_clip: Option<Vec<String>>,
) -> Option<(String, Vec<String>)> {
    let clip_id = clip_id?;
    let static_clip = persona
        .clips
        .get(clip_id)
        .map(|clip| clip.bubbles.clone())
        .unwrap_or_default();
    let mut pool: Vec<String> = Vec::new();
    for text in ai_clip.unwrap_or_default().into_iter().chain(static_clip) {
        if !pool.contains(&text) {
            pool.push(text);
        }
    }
    if pool.is_empty() {
        return None;
    }
    Some((format!("clip:{clip_id}"), pool))
}

/// 自动状态：time 时段优先，否则循环，最后兜底
fn automatic_state(persona: &PersonaConfig, mins: u32, needs: &crate::needs::NeedsState) -> String {
    // 1) 硬时段最高优先（作者写死的作息，比如夜间必须睡觉）
    if let Some(slot) = find_active_slot(&persona.schedule, mins) {
        return slot.state.clone();
    }
    // 2) 需求拉取：饿了去吃饭、累了去休息/睡觉（拉去的状态必须真有素材，否则忽略）
    if let Some(pulled) = persona.needs.pull_state(needs) {
        if state_has_material(persona, &pulled) {
            return pulled.to_string();
        }
    }
    // 3) 按日程循环
    if let Some(s) = loop_state_at(&persona.schedule, mins) {
        return s;
    }
    // 兜底：按日程顺序取第一个状态；
    // 仅当角色完全没有定义状态时才使用硬编码值。
    ordered_state_keys(persona)
        .first()
        .cloned()
        .unwrap_or_else(|| "Awake".into())
}

/// 计算角色的有效显示尺寸：优先用配置文件；为 0（未配置）时按第一个可读动作帧条的
/// 实际尺寸 ÷ frames 计算（clips 模型下同一角色的各动作帧尺寸一致）。
pub fn effective_display_size(persona: &PersonaConfig) -> (u32, u32) {
    let (w, h) = (persona.display_w, persona.display_h);
    if w > 0 && h > 0 {
        return (w, h);
    }
    // 内置角色资源是站点路径（/…），无法直接读文件；导入角色为磁盘绝对路径。
    let mut clips: Vec<&AnimationClipConfig> = persona.clips.values().collect();
    clips.sort_by(|a, b| a.spritesheet.cmp(&b.spritesheet));
    for clip in clips {
        if clip.spritesheet.starts_with('/') {
            continue;
        }
        let Ok(path) = Path::new(&clip.spritesheet).canonicalize() else {
            continue;
        };
        let Ok((sw, sh)) = image::image_dimensions(&path) else {
            continue;
        };
        let frames = clip.frames.max(1);
        let cw = ((sw as f64 / frames as f64).round() as u32).max(1);
        return (cw, sh.max(1));
    }
    (w.max(1), h.max(1))
}

/// 在基准显示尺寸上乘全局缩放 zoom，并四舍五入、下限 1px，防止缩出 0 尺寸。
/// persona.json 的 display_w/display_h 保持基准语义不动，缩放只在读取/渲染时叠加。
pub fn effective_display_size_zoomed(persona: &PersonaConfig, zoom: f64) -> (u32, u32) {
    let (w, h) = effective_display_size(persona);
    (
        ((w as f64 * zoom).round() as u32).max(1),
        ((h as f64 * zoom).round() as u32).max(1),
    )
}

/// 等比适配显示尺寸到上限内（保持宽高比，绝不独立钳制宽高导致拉伸）。
/// 超限时取 `min(max_w/w, max_h/h)` 作为缩放比例，四舍五入且下限 1px；未超限原样返回。
/// max 为 0 时按 1 处理，避免除零。
pub fn fit_display_size_proportional(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let mw = max_w.max(1) as f64;
    let mh = max_h.max(1) as f64;
    let scale = (mw / w as f64).min(mh / h as f64);
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    )
}

/// 从显示器工作区推导角色显示内容上限（逻辑像素）：工作区换算成逻辑像素后减去窗口余量，
/// 保证 persona 窗口整体（含边距、气泡区）放得进工作区。取不到窗口/显示器时不钳制（返回 None）。
fn work_area_content_limit(app: &AppHandle) -> Option<(u32, u32)> {
    let win = app.get_webview_window("persona")?;
    let scale = win.scale_factor().ok()?;
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let wa = monitor.work_area();
    // 物理工作区 ÷ scale 得逻辑像素，再减去水平两侧与底距/顶部余量
    let max_w = (wa.size.width as f64 / scale - (2 * crate::H_MARGIN) as f64).max(1.0) as u32;
    let max_h = (wa.size.height as f64 / scale - (crate::SPRITE_BOTTOM + crate::TOP_MARGIN) as f64)
        .max(1.0) as u32;
    Some((max_w, max_h))
}

/// 计算 time 时段的结束时刻（用于推算下一次状态切换）
fn slot_end_datetime(
    now: &chrono::DateTime<chrono::Local>,
    slot: &TimeSlot,
) -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    let start = parse_mins(&slot.start).unwrap_or(0);
    let end = parse_mins(&slot.end).unwrap_or(0);
    let mins = now_minutes(now);
    let date = now.date_naive();
    let make = |m: u32| -> chrono::DateTime<chrono::Local> {
        if m >= 1440 {
            let next = date + chrono::Days::new(1);
            return chrono::Local
                .from_local_datetime(&next.and_hms_opt(0, 0, 0).unwrap())
                .single()
                .unwrap_or(*now);
        }
        let h = m / 60;
        let min = m % 60;
        chrono::Local
            .from_local_datetime(&date.and_hms_opt(h, min, 0).unwrap())
            .single()
            .unwrap_or(*now)
    };
    let end_dt = make(end);
    if start <= end {
        end_dt
    } else if mins >= start {
        end_dt + chrono::Duration::days(1)
    } else {
        end_dt
    }
}

/// 计算下一次 loop 状态切换的时刻：按「当日分钟数 % 总时长」得到当前循环位置，
/// 推进到下一个条目边界。返回 now 之后最近的边界时刻（跨天时自然落到次日）。
fn next_loop_boundary_time(
    schedule: &ScheduleConfig,
    now: &chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    let entries = schedule.loop_entries();
    if entries.is_empty() {
        return None;
    }
    let total: u64 = entries.iter().map(|e| e.duration as u64).sum();
    if total == 0 {
        return None;
    }
    let pos = (now_minutes(now) as u64) % total;
    let mut acc: u64 = 0;
    for e in entries {
        acc += e.duration as u64;
        if acc > pos {
            let offset_min = acc - pos;
            // 直接用 now + 剩余分钟数：跨天时 chrono 自动进位到次日对应时刻
            return Some(*now + chrono::Duration::minutes(offset_min as i64));
        }
    }
    None
}

/// 计算下一次可能的状态切换时刻（取所有候选的最早者）：
/// 当前 time 时段结束、下一个 loop 边界；都不可得则 now + 30s 兜底。
fn next_transition_at(
    persona: &PersonaConfig,
    now: &chrono::DateTime<chrono::Local>,
) -> chrono::DateTime<chrono::Local> {
    let mut candidates: Vec<chrono::DateTime<chrono::Local>> = Vec::new();

    // 当前 time 时段结束
    let mins = now_minutes(now);
    if let Some(slot) = find_active_slot(&persona.schedule, mins) {
        let end = slot_end_datetime(now, slot);
        if *now < end {
            candidates.push(end);
        }
    }
    // 下一个 loop 边界
    if let Some(boundary) = next_loop_boundary_time(&persona.schedule, now) {
        candidates.push(boundary);
    }

    candidates
        .into_iter()
        .min()
        .unwrap_or_else(|| *now + chrono::Duration::seconds(30))
}

/// 根据 persona 定义、当前状态与**当前正在播放的动作**组装对话用的 LLM system prompt。
/// 语气优先取状态的 `tone`，没写就按 `talkativeness` 推导。
///
/// `activity` 来自后端播放调度（见 [`StateEngine::current_activity`]）。带上它才能保证
/// “说的”和“演的”一致：没有动作上下文时，模型会自己编造画面里没有的动作/物品，
/// 例如问“在干什么”回答“磨剑”，而角色的动作集里根本没有磨剑。
pub fn build_system_prompt(
    persona: &PersonaConfig,
    state: &str,
    activity: Option<&ActivityView>,
) -> String {
    let sp = &persona.system_prompt;
    let state_cfg = persona.states.get(state);
    let label = state_cfg.map(|s| s.label.as_str()).unwrap_or(state);
    let tone = state_cfg
        .map(StateConfig::chat_tone)
        .unwrap_or_else(|| Talkativeness::Normal.chat_tone());
    let action = match activity {
        Some(a) => {
            let clip_label = if a.clip_label.is_empty() {
                a.clip.as_str()
            } else {
                a.clip_label.as_str()
            };
            format!(
                "【当前动作】\n你此刻正在「{clip_label}」。{description}\n\
                 用户若问你在做什么，只能依据这个动作回答；不要提及画面里没有的动作、物品或场景。",
                description = a.clip_description.trim()
            )
        }
        // 没有播放上下文时也必须给约束，否则模型仍会自由编造
        None => {
            "【当前动作】\n此刻画面没有在播放具体动作。不要主动描述动作、物品或场景。".to_string()
        }
    };
    format!(
        "【角色定义】\n{}\n\n【回复风格】\n{}\n\n【当前状态】\n角色当前处于“{}”（{}）状态。{}\n\n{}",
        sp.definition, sp.reply_style, label, state, tone, action
    )
}

fn parse_mins(value: &str) -> Option<u32> {
    let (h, m) = value.split_once(':')?;
    // 严格 HH:MM：两位数字、00:00~24:00（24:00 仅作为“当日终点”合法）
    if h.len() != 2
        || m.len() != 2
        || !h.bytes().all(|b| b.is_ascii_digit())
        || !m.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 24 || m > 59 || (h == 24 && m != 0) {
        return None;
    }
    Some(h * 60 + m)
}

/// xorshift64 状态（惰性以纳秒时间做种子），避免引入 rand 依赖
static RNG_STATE: AtomicU64 = AtomicU64::new(0);

/// 推进并返回下一个伪随机数（PRNG，非密码学安全）
fn rng_next() -> u64 {
    let mut s = RNG_STATE.load(Ordering::Relaxed);
    if s == 0 {
        s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        if s == 0 {
            s = 0x9E37_79B9_7F4A_7C15;
        }
    }
    s ^= s << 13;
    s ^= s >> 7;
    s ^= s << 17;
    if s == 0 {
        s = 0x9E37_79B9_7F4A_7C15;
    }
    RNG_STATE.store(s, Ordering::Relaxed);
    s
}

/// [min, max] 闭区间内均匀取整（max <= min 时返回 min）
pub(crate) fn rng_range_i64(min: i64, max: i64) -> i64 {
    if max <= min {
        return min;
    }
    min + (rng_next() % ((max - min + 1) as u64)) as i64
}

/// [0,1) 之间的随机数（用于速度抖动、概率判定）
pub(crate) fn rng_unit() -> f64 {
    (rng_range_i64(0, 9_999) as f64) / 10_000.0
}

/// Fisher–Yates 洗牌
fn shuffle(list: &mut [String]) {
    for i in (1..list.len()).rev() {
        let j = (rng_next() % ((i + 1) as u64)) as usize;
        list.swap(i, j);
    }
}

/// 用文案池装填洗牌袋：打乱顺序后逐条弹出，保证一轮之内不重复；
/// 池多于一条时，若重洗后的首条与上一轮末条相同则与第二条交换，避免跨轮连续重复。
pub(crate) fn refill_bag(mut pool: Vec<String>, last_shown: Option<&str>) -> VecDeque<String> {
    shuffle(&mut pool);
    if pool.len() > 1 {
        if let Some(last) = last_shown {
            if pool.first().map(String::as_str) == Some(last) {
                pool.swap(0, 1);
            }
        }
    }
    pool.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> PersonaConfig {
        let mut cfg: PersonaConfig =
            serde_json::from_str(include_str!("../../resources/characters/link/persona.json"))
                .expect("内置 persona.json 解析失败");
        cfg.finalize();
        cfg
    }

    /// 取某状态第一条链的第 `index` 段（可变）
    fn segment_mut<'a>(
        p: &'a mut PersonaConfig,
        state: &str,
        index: usize,
    ) -> &'a mut SegmentConfig {
        &mut p.states.get_mut(state).unwrap().chains[0].segments[index]
    }

    #[test]
    fn schedule_parses_new_format() {
        let p = link();
        // 循环需有逐状态条目，且引用的状态都必须存在
        let entries = p.schedule.loop_entries();
        assert!(!entries.is_empty());
        for e in entries {
            assert!(e.duration > 0, "循环时长必须大于 0");
            assert!(p.states.contains_key(&e.state), "循环状态缺失: {}", e.state);
        }
        for slot in p.schedule.time() {
            assert!(
                p.states.contains_key(&slot.state),
                "time 时段状态缺失: {}",
                slot.state
            );
        }
    }

    #[test]
    fn schedule_validation_rejects_bad_references_and_durations() {
        let base = link();
        assert!(validate_persona_structure(&base).is_ok());

        // loop 指向未定义状态：运行时该状态会被静默跳过
        let mut p = base.clone();
        p.schedule.r#loop[0].state = "ghost".into();
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("ghost"), "{error}");

        // loop 时长为 0：该条目永远不会被排到
        let mut p = base.clone();
        p.schedule.r#loop[0].duration = 0;
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("duration"), "{error}");

        // time 指向未定义状态
        let mut p = base.clone();
        p.schedule.time[0].state = "ghost".into();
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("ghost"), "{error}");

        // time 时间格式非法
        let mut p = base.clone();
        p.schedule.time[0].start = "25:99".into();
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("时间格式"), "{error}");
    }

    #[test]
    fn parse_mins_is_strict_about_format_and_range() {
        assert_eq!(parse_mins("00:00"), Some(0));
        assert_eq!(parse_mins("08:05"), Some(8 * 60 + 5));
        assert_eq!(parse_mins("23:59"), Some(23 * 60 + 59));
        // 24:00 作为“当日终点”合法，其余越界/非两位格式一律拒绝
        assert_eq!(parse_mins("24:00"), Some(1440));
        assert_eq!(parse_mins("24:01"), None);
        assert_eq!(parse_mins("8:05"), None);
        assert_eq!(parse_mins("08:5"), None);
        assert_eq!(parse_mins("23:60"), None);
        assert_eq!(parse_mins("ab:cd"), None);
        assert_eq!(parse_mins(""), None);
    }

    #[test]
    fn persona_validation_enforces_fixed_states_and_routine() {
        let base = link();
        assert!(validate_persona_structure(&base).is_ok());

        // 去掉 routine（唯一必配状态）→ 拒绝
        let mut p = base.clone();
        p.states.remove("routine");
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("routine"), "{error}");

        // 自定义状态（不在固定集合内）→ 拒绝
        let mut p = base.clone();
        p.states.insert(
            "custom".into(),
            StateConfig {
                label: "自定义".into(),
                talkativeness: String::new(),
                tone: String::new(),
                bubble_gap_min: None,
                chains: vec![],
            },
        );
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("custom"), "{error}");
    }

    #[test]
    fn schedule_may_reference_unconfigured_canonical_state() {
        // 角色没配 sleep 素材：日程里仍可写 sleep，运行时完全按 routine 处理
        let mut p = link();
        p.states.remove("sleep");
        assert!(validate_persona_structure(&p).is_ok());
        assert_eq!(runtime_state(&p, "sleep"), "routine");
        assert_eq!(runtime_state(&p, "routine"), "routine");

        let full = link();
        assert_eq!(runtime_state(&full, "sleep"), "sleep");
    }

    #[test]
    fn chain_validation_rejects_duplicate_segment_and_empty_state() {
        let base = link();

        // 同一个状态里段的 id 撞车 → 事件里的 scene_id 会指错段
        let mut p = base.clone();
        let first_id = p.states["routine"].chains[0].segments[0].id.clone();
        p.states
            .get_mut("routine")
            .unwrap()
            .chains
            .first_mut()
            .unwrap()
            .segments
            .push(SegmentConfig {
                id: first_id,
                steps: vec![SceneStepConfig {
                    clip: "observe".into(),
                    ..Default::default()
                }],
                ..Default::default()
            });
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("重名的段"), "{error}");

        // 状态没有任何活动 → 永远不播
        let mut p = base.clone();
        p.states.get_mut("routine").unwrap().chains.clear();
        let error = validate_persona_structure(&p).unwrap_err();
        assert!(error.contains("没有可播放的活动"), "{error}");
    }

    #[test]
    fn finalize_derives_ids_labels_and_asset_paths() {
        let mut p = link();
        let expected_sheet = p.clips["walk"].spritesheet.clone();
        // 清掉所有派生字段：段 id/label、链 label、动作资源路径
        for cfg in p.states.values_mut() {
            for chain in cfg.chains.iter_mut() {
                chain.label.clear();
                for segment in chain.segments.iter_mut() {
                    segment.id.clear();
                    segment.label.clear();
                }
            }
        }
        for clip in p.clips.values_mut() {
            clip.spritesheet.clear();
        }
        p.finalize();

        for cfg in p.states.values() {
            for chain in &cfg.chains {
                assert!(!chain.label.is_empty(), "链 label 应由 finalize 补齐");
                for (index, segment) in chain.segments.iter().enumerate() {
                    assert_eq!(segment.id, format!("{}#{index}", chain.id));
                    assert!(!segment.label.is_empty(), "段 label 应由 finalize 补齐");
                }
            }
        }
        assert_eq!(p.clips["walk"].spritesheet, expected_sheet);
        assert!(validate_persona_structure(&p).is_ok());
    }

    #[test]
    fn speech_window_respects_min_action_and_bounds() {
        let start = chrono::Local::now();
        // 过渡动作（<30 秒）不说
        assert!(speech_window(start, start, 20_000, None, None, 6.0).is_none());

        // 首次说话不受 20%~70% 偏好限制：30 秒动作 → [6s, 24s]（结束前 6 秒说完）
        let (from, to) = speech_window(start, start, 30_000, None, None, 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 6);
        assert_eq!((to - start).num_seconds(), 24);

        // 60 秒动作：首次说话 → [12s, 54s]
        let (from, to) = speech_window(start, start, 60_000, None, None, 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 12);
        assert_eq!((to - start).num_seconds(), 54);

        // 距上次说话不足静默上限（E=6 → 12 分钟）：不早于 earliest_at
        let just_spoke = start - chrono::Duration::minutes(1);
        let earliest = start + chrono::Duration::seconds(10);
        let (from, _) =
            speech_window(start, start, 60_000, Some(earliest), Some(just_spoke), 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 12); // max(窗口左端 12s, 10s)

        let earliest = start + chrono::Duration::seconds(30);
        let (from, _) =
            speech_window(start, start, 60_000, Some(earliest), Some(just_spoke), 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 30);

        // 从未说过话（启动 grace）：尊重 earliest_at，右端放宽到"说完为止"
        let earliest = start + chrono::Duration::seconds(30);
        let (from, to) = speech_window(start, start, 60_000, Some(earliest), None, 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 30);
        assert_eq!((to - start).num_seconds(), 54);

        // 首次说话落在动作尾部（45 秒动作、grace 38 秒）→ 仍在实例内能说完
        let earliest = start + chrono::Duration::seconds(38);
        let (from, to) = speech_window(start, start, 45_000, Some(earliest), None, 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 38);
        assert_eq!((to - start).num_seconds(), 39); // 45 - 6

        // 连放宽后都放不下（grace 40 秒 > 39 秒）→ 这个实例不说，等下一个动作
        let earliest = start + chrono::Duration::seconds(40);
        assert!(speech_window(start, start, 45_000, Some(earliest), None, 6.0).is_none());

        // earliest 超过窗口右端 → 这个实例不说
        let earliest = start + chrono::Duration::seconds(50);
        assert!(
            speech_window(start, start, 60_000, Some(earliest), Some(just_spoke), 6.0).is_none()
        );

        // 静默超过上限 → 忽略间隔，直接取窗口左端
        let long_ago = start - chrono::Duration::minutes(30);
        let (from, _) =
            speech_window(start, start, 60_000, Some(earliest), Some(long_ago), 6.0).unwrap();
        assert_eq!((from - start).num_seconds(), 12);
    }

    #[test]
    fn time_slot_overrides_loop() {
        let p = link();
        // 取第一个 time 时段的中点 → 应返回该时段固定的 state
        let slot = p.schedule.time().first().expect("应至少有一个 time 时段");
        let start = parse_mins(&slot.start).unwrap_or(0);
        let end = parse_mins(&slot.end).unwrap_or(0);
        let mid = if start <= end {
            (start + end) / 2
        } else {
            ((start + end + 1440) / 2) % 1440
        };
        let needs = crate::needs::NeedsState::default();
        assert_eq!(automatic_state(&p, mid, &needs), slot.state);
    }

    #[test]
    fn needs_pull_beats_loop_but_loses_to_time_slot() {
        let mut p = link();
        // 12:00 不在任何 time 时段内（link 只在 20:00~08:00 睡觉）→ 需求说了算
        let noon = 12 * 60;
        let looped = loop_state_at(&p.schedule, noon).unwrap();
        let needs = crate::needs::NeedsState::default();
        assert_eq!(automatic_state(&p, noon, &needs), looped);

        // 饿了 → 拉去吃饭（需求优先于循环）
        let hungry = crate::needs::NeedsState {
            hunger: 0.95,
            ..Default::default()
        };
        assert_eq!(automatic_state(&p, noon, &hungry), "eat");

        // 很累 → 拉去睡觉（pull 里 energy 的规则更靠前）
        let mut tired = hungry.clone();
        tired.hunger = 0.2;
        tired.energy = 0.1;
        assert_eq!(automatic_state(&p, noon, &tired), "sleep");

        // 需求拉去的状态必须真有素材，否则忽略
        p.states.remove("eat");
        assert_ne!(automatic_state(&p, noon, &hungry), "eat");

        // 硬时段最高优先：20:00~08:00 是睡眠时段，再饿也得先睡
        assert_eq!(automatic_state(&p, 23 * 60, &hungry), "sleep");
    }

    #[test]
    fn loop_state_at_follows_durations() {
        let p = link();
        let entries = p.schedule.loop_entries();
        // 第 0 分钟 → 第一个状态；第 duration 分钟 → 第二个；首个总时长处 → 回到第一个
        assert_eq!(
            loop_state_at(&p.schedule, 0),
            Some(entries[0].state.clone())
        );
        assert_eq!(
            loop_state_at(&p.schedule, entries[0].duration),
            Some(entries[1].state.clone())
        );
        let total: u64 = entries.iter().map(|e| e.duration as u64).sum();
        assert_eq!(
            loop_state_at(&p.schedule, total as u32),
            Some(entries[0].state.clone())
        );
    }

    #[test]
    fn effective_display_uses_config() {
        let p = link();
        // 显式配置了 display_w/display_h 时应直接返回配置值（而非从精灵图计算）
        assert!(
            p.display_w > 0 && p.display_h > 0,
            "link 应显式配置显示尺寸"
        );
        assert_eq!(effective_display_size(&p), (p.display_w, p.display_h));
    }

    #[test]
    fn effective_display_computes_from_clip_strip() {
        let mut p = link();
        p.display_w = 0;
        p.display_h = 0;
        // 用仓库内真实动作帧条（文件系统可读）：observe 11520×208、60 帧 → 每帧 192×208
        let strip = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/characters/link/clips/observe.webp");
        p.clips.get_mut("observe").unwrap().spritesheet = strip.to_string_lossy().into_owned();
        assert_eq!(effective_display_size(&p), (192, 208));
    }

    #[test]
    fn embedded_clip_strips_match_declared_frame_counts() {
        let p = link();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../resources");
        let mut frame_size: Option<(u32, u32)> = None;
        for (id, clip) in &p.clips {
            assert!(clip.frames > 0 && clip.frame_ms > 0, "动作 {id} 帧参数非法");
            let path = root.join(clip.spritesheet.trim_start_matches('/'));
            let (width, height) = image::image_dimensions(&path)
                .unwrap_or_else(|e| panic!("动作 {id} 读取失败 {}: {e}", path.display()));
            assert_eq!(
                width % clip.frames,
                0,
                "动作 {id} 帧条宽度 {width} 不是帧数 {} 的整数倍",
                clip.frames
            );
            let current = (width / clip.frames, height);
            assert_eq!(
                *frame_size.get_or_insert(current),
                current,
                "动作 {id} 的帧尺寸与其他动作不一致"
            );
        }
    }

    #[test]
    fn zoomed_display_scales_base_and_min_one() {
        let p = link();
        // 2.0 翻倍
        assert_eq!(
            effective_display_size_zoomed(&p, 2.0),
            (p.display_w * 2, p.display_h * 2)
        );
        // 0.5 减半（125 * 0.5 = 62.5，四舍五入为 63）
        assert_eq!(effective_display_size_zoomed(&p, 0.5), (58, 63));
        // zoom=0 时也至少保留 1px，避免缩成 0 导致不可见
        let mut tiny = link();
        tiny.display_w = 1;
        tiny.display_h = 1;
        assert_eq!(effective_display_size_zoomed(&tiny, 0.0), (1, 1));
    }

    #[test]
    fn fit_display_size_proportional_respects_bounds() {
        // 未超限：原样返回
        assert_eq!(fit_display_size_proportional(100, 50, 200, 200), (100, 50));
        // 宽超限：按宽等比缩，高随之变小（保持宽高比 2:1）
        assert_eq!(fit_display_size_proportional(200, 100, 100, 500), (100, 50));
        // 高超限：按高等比缩，宽随之变小（保持 1:2）
        assert_eq!(fit_display_size_proportional(50, 200, 500, 100), (25, 100));
        // 双超限：方形入方形，等比缩到内切
        assert_eq!(
            fit_display_size_proportional(400, 400, 200, 200),
            (200, 200)
        );
        // 上限为 0：不除零，按 max(1) 兜底
        assert_eq!(fit_display_size_proportional(1, 1, 0, 0), (1, 1));
        // 极小值下限：等比缩后高度约 0.3px，须钳制到 1px，避免缩成 0
        assert_eq!(fit_display_size_proportional(1000, 1, 300, 300), (300, 1));
    }

    #[test]
    fn history_id_validation() {
        assert!(is_valid_history_id("link"));
        assert!(is_valid_history_id("local-doraemon"));
        assert!(!is_valid_history_id(""));
        // 路径穿越 / 非法字符应被拒绝（与 remove_persona 校验一致）
        assert!(!is_valid_history_id("../etc/passwd"));
        assert!(!is_valid_history_id("abc/de"));
    }

    #[test]
    fn history_trims_to_last_20_and_filters_roles() {
        let mut msgs: Vec<LlmMessage> = (0..30)
            .map(|i| LlmMessage {
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                content: crate::llm::Content::Text(format!("m{i}")),
            })
            .collect();
        // 追加一条 system，应被角色白名单过滤掉
        msgs.push(LlmMessage {
            role: "system".into(),
            content: crate::llm::Content::Text("sys".into()),
        });
        let kept = sanitize_history(msgs);
        assert_eq!(kept.len(), 20);
        assert!(kept.iter().all(|m| is_valid_history_role(&m.role)));
        assert_eq!(kept.first().unwrap().content.as_text(), "m10");
        assert_eq!(kept.first().unwrap().role, "user");
        assert_eq!(kept.last().unwrap().content.as_text(), "m29");
    }

    /// 构造今天指定 HH:MM（秒为 0）的本地时刻，供 next_transition_at 测试用。
    fn local_at(h: u32, m: u32) -> chrono::DateTime<chrono::Local> {
        use chrono::TimeZone;
        let date = chrono::Local::now().date_naive();
        chrono::Local
            .from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
            .single()
            .unwrap()
    }

    /// 按「当日第几分钟」构造今天的本地时刻
    fn today_at(mins: u32) -> chrono::DateTime<chrono::Local> {
        local_at(mins / 60, mins % 60)
    }

    /// 独立参照：把 loop 边界当作「loop 状态发生变化的下一分钟」扫描出来，
    /// 与生产实现（按累计时长推算）走的是不同路径，可交叉校验。
    fn scanned_loop_boundary(
        schedule: &ScheduleConfig,
        now: &chrono::DateTime<chrono::Local>,
    ) -> chrono::DateTime<chrono::Local> {
        let start = now_minutes(now);
        let current = loop_state_at(schedule, start);
        for offset in 1..=(24 * 60) {
            if loop_state_at(schedule, start + offset) != current {
                return *now + chrono::Duration::minutes(offset as i64);
            }
        }
        panic!("24 小时内没有 loop 状态变化");
    }

    /// 在日程里找一个「位于 time 时段内、且 loop 边界早于时段结束」的时刻
    fn find_slot_with_earlier_loop_boundary(
        persona: &PersonaConfig,
    ) -> (
        chrono::DateTime<chrono::Local>,
        chrono::DateTime<chrono::Local>,
    ) {
        for mins in 0..24 * 60 {
            let Some(slot) = find_active_slot(&persona.schedule, mins) else {
                continue;
            };
            let now = today_at(mins);
            let end = slot_end_datetime(&now, slot);
            if end <= now {
                continue;
            }
            let boundary = scanned_loop_boundary(&persona.schedule, &now);
            if boundary < end {
                return (now, boundary);
            }
        }
        panic!("当前日程里不存在「时段内 loop 边界更早」的时刻");
    }

    #[test]
    fn next_transition_uses_loop_boundary_in_time_slot() {
        let p = link();
        let (now, boundary) = find_slot_with_earlier_loop_boundary(&p);
        assert!(boundary > now, "loop 边界必须在 now 之后");
        assert_eq!(next_transition_at(&p, &now), boundary);
    }

    #[test]
    fn next_transition_uses_time_slot_end() {
        let p = link();
        // 找出「时段结束早于 loop 边界」的时刻，验证时段结束优先
        let found = (0..24 * 60).find_map(|mins| {
            let now = today_at(mins);
            let slot = find_active_slot(&p.schedule, mins)?;
            let end = slot_end_datetime(&now, slot);
            (end > now && end < scanned_loop_boundary(&p.schedule, &now)).then_some((now, end))
        });
        let (now, end) = found.expect("当前日程里不存在「时段结束更早」的时刻");
        assert_eq!(next_transition_at(&p, &now), end);
    }

    #[test]
    fn next_transition_crosses_midnight_boundary() {
        let p = link();
        // 找跨午夜时段内「loop 边界更早」的时刻，验证边界会自然落到次日
        let found = (0..24 * 60).find_map(|mins| {
            let slot = find_active_slot(&p.schedule, mins)?;
            let (start, end_min) = (parse_mins(&slot.start)?, parse_mins(&slot.end)?);
            if start <= end_min {
                return None;
            }
            let now = today_at(mins);
            let end = slot_end_datetime(&now, slot);
            let boundary = scanned_loop_boundary(&p.schedule, &now);
            (boundary < end && boundary.date_naive() > now.date_naive()).then_some((now, boundary))
        });
        let (now, boundary) = found.expect("当前日程里不存在跨午夜且 loop 边界更早的时刻");
        assert_eq!(next_transition_at(&p, &now), boundary);
    }

    #[test]
    fn next_transition_falls_back_to_30s() {
        let mut p = link();
        p.schedule = ScheduleConfig {
            r#loop: vec![],
            time: vec![],
        };
        let now = local_at(9, 0);
        assert_eq!(
            next_transition_at(&p, &now),
            now + chrono::Duration::seconds(30)
        );
    }

    #[test]
    fn talkativeness_parse_is_lenient() {
        assert_eq!(Talkativeness::parse("chatty"), Talkativeness::Chatty);
        assert_eq!(Talkativeness::parse("normal"), Talkativeness::Normal);
        assert_eq!(Talkativeness::parse("quiet"), Talkativeness::Quiet);
        assert_eq!(Talkativeness::parse("mute"), Talkativeness::Mute);
        // 缺省 / 未知值宽松回退 normal，不让整个 persona 解析失败
        assert_eq!(Talkativeness::parse(""), Talkativeness::Normal);
        assert_eq!(Talkativeness::parse("whatever"), Talkativeness::Normal);
    }

    #[test]
    fn state_config_talkativeness_defaults_and_parses() {
        let p = link();
        // link 的 sleep 配了 mute、eat 配了 chatty；其余缺省 normal
        assert_eq!(p.states["sleep"].talkativeness(), Talkativeness::Mute);
        assert_eq!(p.states["eat"].talkativeness(), Talkativeness::Chatty);
        assert_eq!(p.states["routine"].talkativeness(), Talkativeness::Normal);
    }

    #[test]
    fn chat_prompt_uses_tone_then_talkativeness_default() {
        let p = link();

        // 显式写了 tone 的状态用 tone
        let relax = build_system_prompt(&p, "relax", None);
        assert!(relax.contains(&p.states["relax"].tone), "{relax}");

        // 没写 tone 的状态按话痨档位推导：eat=chatty、sleep=mute
        let eat = build_system_prompt(&p, "eat", None);
        assert!(eat.contains(Talkativeness::Chatty.chat_tone()), "{eat}");
        let sleep = build_system_prompt(&p, "sleep", None);
        assert!(sleep.contains(Talkativeness::Mute.chat_tone()), "{sleep}");
        assert!(sleep.contains("梦话"), "{sleep}");

        // 角色定义与回复风格始终在
        assert!(eat.contains(&p.system_prompt.definition), "{eat}");
        assert!(eat.contains(&p.system_prompt.reply_style), "{eat}");

        // 未知状态退回 normal 语气，不 panic
        let unknown = build_system_prompt(&p, "不存在的状态", None);
        assert!(
            unknown.contains(Talkativeness::Normal.chat_tone()),
            "{unknown}"
        );
    }

    #[test]
    fn chat_prompt_grounds_reply_in_current_activity() {
        let p = link();
        let activity = ActivityView {
            state: "focus".into(),
            scene_id: "maintain_the_shield".into(),
            scene_label: "保养盾牌".into(),
            clip: "polish_shield".into(),
            clip_label: p.clips["polish_shield"].label.clone(),
            clip_description: p.clips["polish_shield"].description.clone(),
        };
        let prompt = build_system_prompt(&p, "focus", Some(&activity));
        // 当前动作的名称与画面描述必须进入提示词
        assert!(prompt.contains("保养盾牌"), "{prompt}");
        assert!(prompt.contains(&activity.clip_description), "{prompt}");
        // 并且明确禁止编造画面里没有的动作/物品
        assert!(prompt.contains("不要提及画面里没有的动作"), "{prompt}");

        // 没有播放上下文时同样给出“不要描述动作”的约束
        let idle_prompt = build_system_prompt(&p, "focus", None);
        assert!(idle_prompt.contains("不要主动描述动作"), "{idle_prompt}");
    }

    #[test]
    fn embedded_activities_reference_existing_clips() {
        for (id, json) in EMBEDDED_PERSONAS {
            let mut persona: PersonaConfig = serde_json::from_str(json).unwrap();
            persona.finalize();
            for (state, cfg) in &persona.states {
                assert!(
                    !cfg.chains.is_empty(),
                    "{id}: 状态 {state} 没有任何活动链"
                );
                for chain in &cfg.chains {
                    assert!(!chain.segments.is_empty(), "{id}: 链 {} 没有段", chain.id);
                    for segment in &chain.segments {
                        assert!(
                            !segment.steps.is_empty(),
                            "{id}: 段 {} 没有动作步骤",
                            segment.id
                        );
                        for step in &segment.steps {
                            assert!(
                                persona.clips.contains_key(&step.clip),
                                "{id}: 段 {} 引用未知动作 {}",
                                segment.id,
                                step.clip
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn structure_validation_rejects_incomplete_personas() {
        let p = link();
        assert!(validate_persona_structure(&p).is_ok());
        let mut no_clips = p.clone();
        no_clips.clips.clear();
        assert!(validate_persona_structure(&no_clips).is_err());
        let mut no_activities = p.clone();
        for cfg in no_activities.states.values_mut() {
            cfg.chains.clear();
        }
        assert!(validate_persona_structure(&no_activities).is_err());
        // 任一状态没有任何活动都视为不可播放（前端不做兜底假设）
        let mut state_without_activity = p.clone();
        state_without_activity.states.get_mut("sleep").unwrap().chains.clear();
        assert!(validate_persona_structure(&state_without_activity).is_err());
        // 段引用了不存在的动作：这正是"加载侧宽松"时会被静默过滤、什么都不播的情况
        let mut bad_step = p;
        segment_mut(&mut bad_step, "relax", 0).steps[0].clip = "不存在的动作".into();
        let error = validate_persona_structure(&bad_step).unwrap_err();
        assert!(error.contains("未知动作"), "{error}");
    }

    #[test]
    fn structure_validation_checks_acknowledge_config() {
        let p = link();
        let ack = &p.acknowledge;
        assert!(
            ack.steps_for("routine", "click").is_some(),
            "内置角色应配了「注意你」的反应"
        );
        assert!(
            ack.steps_for("routine", "hover").is_some(),
            "凑近也应有反应"
        );
        assert!(
            ack.steps_for("sleep", "talk").is_some(),
            "睡觉时被打扰应配成「翻个身」而不是不理会"
        );
        assert!(
            ack.steps_for("routine", "talk").is_some(),
            "搭话应有反应（按来源覆盖）"
        );

        // 引用了不存在的动作 → 结构校验拦下
        let mut bad = link();
        bad.acknowledge.by_kind.insert(
            "click".to_string(),
            vec![SceneStepConfig {
                clip: "不存在的动作".into(),
                seconds: Some(crate::engine::SecondsSpec::Fixed(3)),
                ..Default::default()
            }],
        );
        let error = validate_persona_structure(&bad).unwrap_err();
        assert!(error.contains("acknowledge"), "{error}");

        // 未知来源 / 未知状态键 → 同样拦下
        let mut bad_key = link();
        bad_key.acknowledge.by_state.insert("walking".to_string(), vec![]);
        let error = validate_persona_structure(&bad_key).unwrap_err();
        assert!(error.contains("未知状态"), "{error}");
        let mut bad_kind = link();
        bad_kind.acknowledge.by_kind.insert("poke".to_string(), vec![]);
        let error = validate_persona_structure(&bad_kind).unwrap_err();
        assert!(error.contains("未知来源"), "{error}");
    }

    #[test]
    fn ack_speech_window_allows_short_reactions() {
        let start = local_at(10, 0);
        // 5 秒的反应：说话窗口落在 25% 处、结尾留 1.5 秒（常规规则要求 ≥30 秒，这里放宽）
        let (from, to) = ack_speech_window(start, 5_000).unwrap();
        assert_eq!((from - start).num_milliseconds(), 1_250);
        let ends_at = start + chrono::Duration::milliseconds(5_000);
        assert_eq!((ends_at - to).num_milliseconds(), 1_500);
        // 太短的过渡动作仍不配台词
        assert!(ack_speech_window(start, 2_000).is_none());
    }

    #[test]
    fn speak_at_is_kept_only_inside_current_segment() {
        let start = local_at(10, 0);
        // 段内 → 保持（一段最多一句，段内换拍不重新随机）
        assert!(speak_at_in_segment(
            Some(start + chrono::Duration::seconds(20)),
            start,
            40_000
        ));
        // 落在段外（段已结束）→ 需要重新排
        assert!(!speak_at_in_segment(
            Some(start + chrono::Duration::seconds(41)),
            start,
            40_000
        ));
        assert!(!speak_at_in_segment(None, start, 40_000));
    }

    #[test]
    fn step_validation_checks_seconds_range_and_chance() {
        // 区间非法（min > max）
        let mut bad_range = link();
        segment_mut(&mut bad_range, "routine", 0).steps[0].seconds =
            Some(SecondsSpec::Range([10, 5]));
        assert!(validate_persona_structure(&bad_range).is_err());

        // 概率越界
        let mut bad_chance = link();
        segment_mut(&mut bad_chance, "routine", 0).steps[0].chance = 1.5;
        assert!(validate_persona_structure(&bad_chance).is_err());

        // 合法区间通过
        let mut ok = link();
        segment_mut(&mut ok, "routine", 0).steps[0].seconds = Some(SecondsSpec::Range([5, 10]));
        assert!(validate_persona_structure(&ok).is_ok());
    }

    #[test]
    fn bubble_pool_uses_current_clip_only() {
        let p = link();
        let clip = p.clips.get("walk").unwrap();
        assert!(!clip.bubbles.is_empty(), "内置角色每个动作都应带静态文案");

        // 没有 AI 台词时用动作自带文案
        let (key, pool) = resolve_bubble_pool(&p, Some("walk"), None).unwrap();
        assert_eq!(key, "clip:walk");
        assert_eq!(pool, clip.bubbles);

        // 当日 AI 台词与动作自带文案合并（AI 在前），避免 AI 只产出 1 条时反复念同一句
        let (key, pool) =
            resolve_bubble_pool(&p, Some("walk"), Some(vec!["AI 动作台词".into()])).unwrap();
        assert_eq!(key, "clip:walk");
        let mut expected = vec!["AI 动作台词".to_string()];
        expected.extend(clip.bubbles.iter().cloned());
        assert_eq!(pool, expected);

        // AI 与自带文案重复时只保留一条
        let duplicated = resolve_bubble_pool(&p, Some("walk"), Some(clip.bubbles.clone())).unwrap();
        assert_eq!(duplicated.1, clip.bubbles);
    }

    #[test]
    fn bubble_pool_is_none_without_clip_or_text() {
        let mut p = link();
        // 没有正在播放的动作 → 不弹（文案只挂在动作上，绝不猜）
        assert!(resolve_bubble_pool(&p, None, None).is_none());

        // 动作没有自带文案且当天没有 AI 台词 → 不弹
        p.clips.get_mut("walk").unwrap().bubbles.clear();
        assert!(resolve_bubble_pool(&p, Some("walk"), None).is_none());
        assert!(resolve_bubble_pool(&p, Some("walk"), Some(vec![])).is_none());

        // 引用了不存在的动作 id 同样不弹（手改配置兜底）
        assert!(resolve_bubble_pool(&p, Some("not-a-clip"), None).is_none());
    }

    #[test]
    fn state_clip_ids_are_unique_and_in_scene_order() {
        let p = link();
        let ids = state_clip_ids(&p, "relax");
        assert!(!ids.is_empty());
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "同一动作只应出现一次");
        for id in &ids {
            assert!(p.clips.contains_key(id), "引用了不存在的动作: {id}");
        }
        assert!(state_clip_ids(&p, "不存在的状态").is_empty());
    }

    #[test]
    fn default_gap_follows_talkativeness_and_mute_is_none() {
        assert_eq!(default_gap_min(Talkativeness::Chatty), Some(3.0));
        assert_eq!(default_gap_min(Talkativeness::Normal), Some(6.0));
        assert_eq!(default_gap_min(Talkativeness::Quiet), Some(15.0));
        assert_eq!(default_gap_min(Talkativeness::Mute), None);
        // 静默上限 = 2×E，且不超过 15 分钟
        assert_eq!(max_silence_min(3.0), 6.0);
        assert_eq!(max_silence_min(6.0), 12.0);
        assert_eq!(max_silence_min(15.0), 15.0);
    }

    #[test]
    fn sampled_gap_stays_within_expected_band() {
        // sample_gap_min 是随机采样，但必须落在 [0.6E, 1.4E]（分钟换算后按秒四舍五入）
        for e in [3.0_f64, 6.0, 15.0] {
            let low = (e * 0.6 * 60.0).round() as i64;
            let high = (e * 1.4 * 60.0).round() as i64;
            for _ in 0..50 {
                let seconds = (sample_gap_min(e) * 60.0).round() as i64;
                assert!(
                    (low..=high).contains(&seconds),
                    "E={e} 采样 {seconds} 秒超出 [{low}, {high}]"
                );
            }
        }
    }

    #[test]
    fn refill_bag_avoids_cross_round_repeat() {
        // 池 >1 时重洗后的首条不得等于上一轮末条（多次运行覆盖洗牌随机性）
        for _ in 0..50 {
            let bag = refill_bag(vec!["a".to_string(), "b".to_string()], Some("a"));
            assert_eq!(bag.front().map(String::as_str), Some("b"));
        }
        // 单条池无法避免重复，原样返回
        let bag = refill_bag(vec!["only".to_string()], Some("only"));
        assert_eq!(bag.front().map(String::as_str), Some("only"));
    }

    #[test]
    fn refill_bag_keeps_all_texts() {
        let pool: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let mut sorted: Vec<String> = refill_bag(pool.clone(), None).into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, pool);
    }

    #[test]
    fn rng_range_stays_within_bounds() {
        for _ in 0..200 {
            let v = rng_range_i64(4, 7);
            assert!((4..=7).contains(&v));
        }
        assert_eq!(rng_range_i64(5, 5), 5);
        assert_eq!(rng_range_i64(7, 4), 7);
    }
}
