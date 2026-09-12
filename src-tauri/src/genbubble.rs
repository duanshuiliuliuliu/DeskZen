//! 每日 AI 气泡生成：用大模型按角色设定 + 当前状态生成各状态的气泡台词，
//! 每天每个动作一批（默认 5 条），存独立缓存文件（不改写 persona.json，原配置永远只读）。
//! 只生成「当前状态可能播到的动作」，随作息推进逐步覆盖，控制每日调用量。
//! 未配置大模型 / 生成失败 / 超长重复校验不通过时，回退动作自带 bubbles。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::engine::{AnimationClipConfig, PersonaConfig, StateEngine};
use crate::llm::LlmMessage;

/// 提示词要求的字数上限（留余量），两行气泡的几何上限见 BUBBLE_MAX_UNITS。
const PROMPT_CHAR_LIMIT: usize = 28;
/// 气泡几何上限：max-width 236px - 内边距 24px = 212px，字号 13px → 每行约 16 个全角字符，
/// 两行 = 32 个全角字符（半角字符折半计）。超过即整条拒绝重试，绝不截断。
const BUBBLE_MAX_UNITS: f64 = 32.0;
/// 每个动作每天生成的台词条数：动作池比状态池细，条数略降以控制成本
const DAILY_LINES_PER_CLIP: usize = 4;
/// 生成历史保留条数（查重用，跨天累计）
pub(crate) const HISTORY_KEEP: usize = 60;
/// 查重时随提示词下发的近期台词条数
const HISTORY_IN_PROMPT: usize = 10;

/// 每日生成缓存：每个角色一个文件（genbubbles/{id}.json）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenBubbleCache {
    /// 缓存归属日期 "YYYY-MM-DD"；非当日内容一律不使用（回退原配置）
    #[serde(default)]
    pub date: String,
    /// clip id -> 当日已生成的台词池（环境气泡逐条弹出，取完当天不再有 AI 文案）
    #[serde(default)]
    pub by_clip: HashMap<String, Vec<String>>,
    /// 当日生成失败的动作（网络错误/校验不过），当天不再重试
    #[serde(default)]
    pub failed: Vec<String>,
    /// 最近生成的台词（跨天累计），用于查重
    #[serde(default)]
    pub history: Vec<String>,
}

/// 内存态：缓存 + 归属角色（切换角色时整体换掉）
pub struct GenState {
    pub persona_id: String,
    pub cache: GenBubbleCache,
}

/// 候选台词校验失败原因（决定重试提示语）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    Empty,
    TooLong,
    Duplicate,
}

/// 当天日期串（本地时区）
pub fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// 视觉宽度估算：CJK/全角字符记 1，其余（半角字母数字标点）记 0.5。
/// 覆盖 CJK 统一表意、假名、谚文、全角标点等常见区间，足够气泡长度判断使用。
pub fn visual_width(s: &str) -> f64 {
    s.chars()
        .map(|c| {
            let u = c as u32;
            let wide = (0x2E80..=0x9FFF).contains(&u)
                || (0xAC00..=0xD7A3).contains(&u)
                || (0xF900..=0xFAFF).contains(&u)
                || (0xFE30..=0xFE4F).contains(&u)
                || (0xFF00..=0xFF60).contains(&u)
                || (0xFFE0..=0xFFE6).contains(&u);
            if wide {
                1.0
            } else {
                0.5
            }
        })
        .sum()
}

/// 清洗模型返回：去首尾空白与成对包裹引号，换行折叠为空格，连续空白压成一个。
pub fn normalize_text(raw: &str) -> String {
    let mut t = raw.trim();
    for (open, close) in [("\"", "\""), ("“", "”"), ("「", "」"), ("《", "》")] {
        if t.starts_with(open) && t.ends_with(close) && t.chars().count() > 2 {
            t = t[open.len()..].trim_end();
            t = t.trim_end_matches(close).trim();
        }
    }
    let mut out = String::with_capacity(t.len());
    let mut prev_space = false;
    for c in t.chars() {
        if c == '\n' || c == '\r' || c == '\t' || c == ' ' {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim_end().to_string()
}

/// 校验候选台词：空 / 超两行几何上限 / 与近期历史重复 都整条拒绝（绝不截断）。
pub fn validate_candidate(text: &str, history: &[String]) -> Result<String, RejectReason> {
    if text.is_empty() {
        return Err(RejectReason::Empty);
    }
    if visual_width(text) > BUBBLE_MAX_UNITS {
        return Err(RejectReason::TooLong);
    }
    if history.iter().any(|h| h == text) {
        return Err(RejectReason::Duplicate);
    }
    Ok(text.to_string())
}

/// 超限/重复后的重试提示语
fn corrective(reason: &RejectReason) -> String {
    match reason {
        RejectReason::TooLong => format!(
            "上一条超过了 {PROMPT_CHAR_LIMIT} 个字的长度限制。请重写一句不超过 {PROMPT_CHAR_LIMIT} 个字的完整台词，句子必须完整，不要截断。"
        ),
        RejectReason::Duplicate => {
            "上一条与近期台词重复了。请换一个全新的说法。".to_string()
        }
        RejectReason::Empty => "上一条是空的。请输出一句台词。".to_string(),
    }
}

/// 动作的画面描述：优先用 clips[].description，缺失时退回 label / 动作 id
fn clip_scene(clip_id: &str, clip: &AnimationClipConfig) -> String {
    if !clip.description.is_empty() {
        return clip.description.clone();
    }
    if !clip.label.is_empty() {
        return format!("角色正在「{}」", clip.label);
    }
    format!("角色正在做动作 {clip_id}")
}

/// 系统提示词：角色定义 + 表达风格 + **当前动作的画面描述**。
///
/// 刻意不带状态的 `tone` / `talkativeness` 语气：那是"聊天时怎么说话"的约束，
/// 台词生成必须锚定在具体动作上，否则会生成"状态说得通、但画面里没这回事"的台词。
fn build_system_prompt(
    persona: &PersonaConfig,
    clip_id: &str,
    clip: &AnimationClipConfig,
) -> String {
    format!(
        "你是桌面宠物角色「{}」的台词作者。\n角色设定：{}\n表达风格：{}\n{}{}\n只能写这一刻（这个动作进行中）说得通的话：不要提别的动作、别的时间或画面里没有的东西。",
        persona.name,
        persona.system_prompt.definition,
        persona.system_prompt.reply_style,
        clip_scene(clip_id, clip),
        if clip.label.is_empty() {
            String::new()
        } else {
            format!("（动作名：{}）", clip.label)
        }
    )
}

/// 用户提示词：任务要求 + 查重列表（近期生成历史 + 该动作自带文案）
fn build_user_prompt(clip_id: &str, clip: &AnimationClipConfig, history: &[String]) -> String {
    let mut avoid: Vec<&String> = history
        .iter()
        .rev()
        .take(HISTORY_IN_PROMPT)
        .collect::<Vec<_>>();
    avoid.reverse();
    for b in &clip.bubbles {
        avoid.push(b);
    }
    let list = if avoid.is_empty() {
        "（暂无）".to_string()
    } else {
        avoid
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "请为下面这个画面写 {} 句气泡台词：{}。每句不超过 {} 个汉字，口语自然，符合角色人设，只写这个动作进行时说得通的话。每句单独一行，不要编号，不要引号和任何说明。不要与以下近期台词重复：\n{}",
        DAILY_LINES_PER_CLIP,
        clip_scene(clip_id, clip),
        PROMPT_CHAR_LIMIT,
        list
    )
}

/// 缓存文件路径：{app_config}/genbubbles/{persona_id}.json
fn cache_file_path(app: &AppHandle, persona_id: &str) -> Result<std::path::PathBuf, String> {
    if !crate::engine::is_valid_history_id(persona_id) {
        return Err(format!("非法角色 id：{persona_id}"));
    }
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("genbubbles").join(format!("{persona_id}.json")))
}

/// 读取缓存；任何失败返回默认空缓存（回退原配置，不报错）
pub fn load(app: &AppHandle, persona_id: &str) -> GenBubbleCache {
    let Ok(path) = cache_file_path(app, persona_id) else {
        return GenBubbleCache::default();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return GenBubbleCache::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 保存缓存（原子写，见 util::atomic_write）
pub(crate) fn save(
    app: &AppHandle,
    persona_id: &str,
    cache: &GenBubbleCache,
) -> Result<(), String> {
    let path = cache_file_path(app, persona_id)?;
    let json = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    crate::util::atomic_write(&path, &json)
}

/// 解析模型输出：逐行清洗校验，返回合格台词（批内去重）与首个拒绝原因（供重试反馈）
fn parse_lines(raw: &str, history: &[String]) -> (Vec<String>, Option<RejectReason>) {
    let mut lines: Vec<String> = Vec::new();
    let mut first_reject: Option<RejectReason> = None;
    for line in raw.lines() {
        let text = normalize_text(line);
        if text.is_empty() {
            continue;
        }
        match validate_candidate(&text, history) {
            // 批内重复直接跳过（模型偶尔输出相邻变体）
            Ok(t) if !lines.contains(&t) => lines.push(t),
            Ok(_) => {}
            Err(reason) => {
                if first_reject.is_none() {
                    first_reject = Some(reason);
                }
            }
        }
    }
    (lines, first_reject)
}

/// 单个动作的台词生成（一次调用批量生成 DAILY_LINES_PER_CLIP 句）：
/// 返回通过校验的台词列表；全部不可用（网络错误或两轮校验都无合格台词）返回 None。
async fn generate_lines(
    cfg: &crate::llm::LlmConfig,
    persona: &PersonaConfig,
    clip_id: &str,
    clip: &AnimationClipConfig,
    history: &[String],
) -> Option<Vec<String>> {
    let system = build_system_prompt(persona, clip_id, clip);
    let mut messages = vec![
        LlmMessage {
            role: "system".into(),
            content: system.into(),
        },
        LlmMessage {
            role: "user".into(),
            content: build_user_prompt(clip_id, clip, history).into(),
        },
    ];
    // 最多一次带反馈的重试：长度/重复问题模型通常一条反馈即可修正
    for _ in 0..2 {
        // 批量生成不需要边出边显示，复用唯一一条流式通道（回调留空）
        let raw = crate::llm::chat_completion_stream(cfg, &cfg.model, &messages, |_| {})
            .await
            .ok()?;
        let (lines, first_reject) = parse_lines(&raw, history);
        // 有合格台词即收下（不足 N 条也够用，环境气泡会回退静态池补位）
        if !lines.is_empty() {
            return Some(lines);
        }
        // 整批无一合格 → 带反馈重试
        let reason = first_reject.unwrap_or(RejectReason::Empty);
        messages.push(LlmMessage {
            role: "assistant".into(),
            content: raw.into(),
        });
        messages.push(LlmMessage {
            role: "user".into(),
            content: corrective(&reason).into(),
        });
    }
    None
}

/// 为「当前状态」生成当日台词：只处理该状态可能播到的动作，逐个生成。
/// 每个动作完成即落盘（中断不丢已完成部分）；检测到角色/状态变化则中止剩余动作。
pub async fn generate_for_current_state(app: &AppHandle) {
    let engine = app.state::<StateEngine>();
    let persona = engine.persona();
    let persona_id = persona.id.clone();
    let state = engine.current_state();
    let llm_cfg = crate::llm::load_config(app);
    if llm_cfg.api_key.is_empty() {
        return;
    }
    // 生成按「动作」而不是「状态」：提示词只带该动作的画面描述，台词才不会跑偏；
    // 状态在这里只用来决定"这一轮该提前准备哪些动作"。
    for clip_id in crate::engine::state_clip_ids(&persona, &state) {
        // 切换角色/状态后中止：剩余动作属于新的日程了
        if engine.persona_id() != persona_id || engine.current_state() != state {
            break;
        }
        if engine.gen_has_today(&clip_id) || engine.gen_failed(&persona_id, &clip_id) {
            continue;
        }
        let clip = match persona.clips.get(&clip_id) {
            Some(clip) => clip.clone(),
            None => continue,
        };
        let history = engine.gen_history();
        match generate_lines(&llm_cfg, &persona, &clip_id, &clip, &history).await {
            Some(texts) => engine.record_gen_bubble(&persona_id, &clip_id, &texts),
            None => engine.record_gen_failure(&persona_id, &clip_id),
        }
    }
}

/// 触发入口：满足条件（开关开、当前状态有用到的动作缺当日文案、已配置 Key、无并发任务）时
/// 后台起一个生成任务。幂等，可在启动/切角色/状态切换/开开关处随意调用。
pub fn maybe_spawn_for_state(app: &AppHandle) {
    let engine = app.state::<StateEngine>();
    if engine.generating.load(Ordering::Relaxed) {
        return;
    }
    if !engine.prefs().ai_bubbles {
        return;
    }
    let state = engine.current_state();
    if !engine.gen_needs_refresh(&state) {
        return;
    }
    // API Key 判断放最后：load_config 要读一次磁盘
    if crate::llm::load_config(app).api_key.is_empty() {
        return;
    }
    if engine
        .generating
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        // Drop 兜底：即使任务 panic 也把 generating 标志复位，避免当天再也不生成
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }
        let engine = app2.state::<StateEngine>();
        let _reset = Reset(&engine.generating);
        generate_for_current_state(&app2).await;
    });
}

/// 从内存 GenState 取某动作当日的生成台词池；非当日 / 未生成 / 空池返回 None（回退原配置）。
/// 供引擎装填气泡洗牌袋时调用，锁内只做查表。
pub fn today_pool(gs: &Mutex<GenState>, clip_id: &str) -> Option<Vec<String>> {
    let gs = crate::util::lock(gs);
    if gs.cache.date != today_str() {
        return None;
    }
    gs.cache
        .by_clip
        .get(clip_id)
        .filter(|v| !v.is_empty())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link_persona() -> PersonaConfig {
        serde_json::from_str(include_str!("../../resources/characters/link/persona.json")).unwrap()
    }

    #[test]
    fn generation_prompt_is_clip_level() {
        let persona = link_persona();
        let clip = persona.clips.get("walk").unwrap();
        let system = build_system_prompt(&persona, "walk", clip);
        // 提示词必须锚定在当前动作的画面上
        assert!(system.contains(&clip.description), "{system}");
        assert!(system.contains(&clip.label), "{system}");
        // 不能混入对话语气（tone / talkativeness 推导结果）：那是"聊天时怎么说话"的约束
        for state_cfg in persona.states.values() {
            assert!(
                !system.contains(state_cfg.chat_tone()),
                "气泡提示词混入对话语气：{}",
                state_cfg.chat_tone()
            );
        }
        let user = build_user_prompt("walk", clip, &[]);
        assert!(user.contains(&clip.description), "{user}");
        assert!(
            user.contains(&clip.bubbles[0]),
            "查重列表应含动作自带文案：{user}"
        );
    }

    #[test]
    fn clip_scene_falls_back_to_label_then_id() {
        let mut clip = AnimationClipConfig {
            spritesheet: String::new(),
            label: "走动".into(),
            description: String::new(),
            frames: 1,
            frame_ms: 100,
            bubbles: vec![],
        };
        assert_eq!(clip_scene("walk", &clip), "角色正在「走动」");
        clip.label.clear();
        assert_eq!(clip_scene("walk", &clip), "角色正在做动作 walk");
        clip.description = "你在桌面上来回走".into();
        assert_eq!(clip_scene("walk", &clip), "你在桌面上来回走");
    }

    #[test]
    fn visual_width_counts_cjk_double() {
        assert_eq!(visual_width("你好"), 2.0);
        assert_eq!(visual_width("abc"), 1.5);
        assert_eq!(visual_width("你好abc"), 3.5);
        assert_eq!(visual_width("，。！"), 3.0);
        assert_eq!(visual_width(""), 0.0);
    }

    #[test]
    fn validate_rejects_overlong_and_duplicates() {
        let history = vec!["今天也要加油鸭".to_string()];
        // 32 个全角字符 = 两行几何上限内，通过
        let ok32: String = "好".repeat(32);
        assert!(validate_candidate(&ok32, &[]).is_ok());
        // 33 个全角字符 = 超两行，整条拒绝
        let too_long: String = "好".repeat(33);
        assert_eq!(
            validate_candidate(&too_long, &[]).unwrap_err(),
            RejectReason::TooLong
        );
        // 半角折半：64 个半角字符恰好 = 32 单位，通过
        let ok64 = "a".repeat(64);
        assert!(validate_candidate(&ok64, &[]).is_ok());
        // 重复
        assert_eq!(
            validate_candidate("今天也要加油鸭", &history).unwrap_err(),
            RejectReason::Duplicate
        );
        // 空
        assert_eq!(
            validate_candidate("", &history).unwrap_err(),
            RejectReason::Empty
        );
    }

    #[test]
    fn normalize_strips_quotes_and_newlines() {
        assert_eq!(normalize_text("  “你好呀”  "), "你好呀");
        assert_eq!(normalize_text("「你好\n呀」"), "你好 呀");
        assert_eq!(normalize_text("a  b\tc\nd"), "a b c d");
        assert_eq!(normalize_text("plain"), "plain");
    }

    #[test]
    fn parse_lines_filters_and_dedupes() {
        let history = vec!["旧台词".to_string()];
        let raw = "第一句\n  \n第二句\n第一句\n\"第三句\"\n旧台词\n";
        let (lines, reject) = parse_lines(raw, &history);
        // 空行清洗、批内去重、引号包裹剥离、与历史重复剔除
        assert_eq!(lines, vec!["第一句", "第二句", "第三句"]);
        // “旧台词”命中历史查重 → 记录拒绝原因供重试反馈（批内已有合格台词，不影响收下）
        assert_eq!(reject, Some(RejectReason::Duplicate));
    }

    #[test]
    fn parse_lines_reports_first_reject_reason() {
        let too_long: String = "好".repeat(40);
        let raw = format!("合格句\n{too_long}");
        let (lines, reject) = parse_lines(&raw, &[]);
        assert_eq!(lines, vec!["合格句"]);
        assert_eq!(reject, Some(RejectReason::TooLong));
    }

    #[test]
    fn cache_round_trips_with_defaults() {
        let cache = GenBubbleCache {
            date: "2026-09-06".into(),
            by_clip: HashMap::from([("Awake".to_string(), vec!["早安".to_string()])]),
            failed: vec!["Sleep".to_string()],
            history: vec!["早安".to_string()],
        };
        let json = serde_json::to_string(&cache).unwrap();
        let back: GenBubbleCache = serde_json::from_str(&json).unwrap();
        assert_eq!(back.date, "2026-09-06");
        assert_eq!(
            back.by_clip.get("Awake").and_then(|v| v.first()),
            Some(&"早安".to_string())
        );
        // 旧文件缺字段：serde default 兜底
        let old: GenBubbleCache = serde_json::from_str("{}").unwrap();
        assert!(old.by_clip.is_empty() && old.failed.is_empty());
    }
}
