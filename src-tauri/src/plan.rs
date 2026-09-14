//! 每日计划：每天给角色安排一个「今天想怎么过」的主题，并让它自己说出来。
//!
//! 每个角色每天只调用一次大模型，拿到三样东西：
//! - `theme`：一句话主题（进日志/面板，也留给第二天"换个不一样的"）；
//! - `say`：角色当天第一次开口时说的话（否则计划只活在权重里，用户看不见）；
//! - `focus` / `avoid`：今天想多做 / 少做的**活动标签**，直接变成链权重倍率。
//!
//! 没配大模型 / 生成失败 / 当天已有计划 → 静默跳过，编排退回「需求 + 权重」。
//! 标签只能从角色链上已有的 tags 里挑，模型凑不出新玩法，也就不会写出画面里没有的东西。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::engine::{PersonaConfig, StateEngine};
use crate::genbubble::{today_str, visual_width};
use crate::llm::LlmMessage;

/// 角色开口那句的长度上限（视觉宽度单位，与气泡两行几何上限一致）
const SAY_MAX_UNITS: f64 = 28.0;
/// 主题长度上限（只进日志/面板，比台词宽松）
const THEME_MAX_UNITS: f64 = 40.0;
/// 一天最多挑几个标签：挑太多等于没挑
const MAX_TAGS: usize = 3;
/// 跨天保留的近期主题条数（提示词里用来避免"天天一个样"）
pub(crate) const RECENT_KEEP: usize = 5;

/// 当日计划内容
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyPlan {
    /// 一句话主题
    #[serde(default)]
    pub theme: String,
    /// 角色当天第一次开口说的话
    #[serde(default)]
    pub say: String,
    /// 今天想多做的活动标签
    #[serde(default)]
    pub focus: Vec<String>,
    /// 今天想少做的活动标签
    #[serde(default)]
    pub avoid: Vec<String>,
}

/// 计划缓存文件：今天的计划 + 今天是否已经试过但失败 + 最近几天的主题
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanCache {
    /// 归属日期 "YYYY-MM-DD"；非当日一律不使用
    #[serde(default)]
    pub date: String,
    /// 当天生成失败过：当天不再重试（避免每 15 分钟打一次接口）
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub plan: Option<DailyPlan>,
    /// 最近几天的主题（跨天累计），提示词里用来要求"换个不一样的"
    #[serde(default)]
    pub recent: Vec<String>,
}

/// 内存态：缓存 + 归属角色（切换角色时整体换掉）
pub struct PlanState {
    pub persona_id: String,
    pub cache: PlanCache,
}

/// 角色身上实际存在的标签 → 示例链名（提示词里给模型挑，避免它自造标签）
pub fn tag_options(persona: &PersonaConfig) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for cfg in persona.states.values() {
        for chain in &cfg.chains {
            for tag in &chain.tags {
                out.entry(tag.clone())
                    .or_insert_with(|| chain.label.clone());
            }
        }
    }
    out
}

/// 系统提示词：角色设定 + 表达风格。计划必须落在这个角色"本来就会做的事"里。
fn build_system_prompt(persona: &PersonaConfig) -> String {
    format!(
        "你是桌面宠物角色「{}」的日程作者。\n角色设定：{}\n表达风格：{}\n只安排它本来就会做的事，不要把画面里没有的东西写进去，也不要给它安排新玩法。",
        persona.name, persona.system_prompt.definition, persona.system_prompt.reply_style
    )
}

/// 用户提示词：可选标签 + 近期主题（要求换一个）+ 输出格式
fn build_user_prompt(options: &BTreeMap<String, String>, recent: &[String]) -> String {
    let list = if options.is_empty() {
        "（这个角色没有可用标签，focus 和 avoid 都留空数组）".to_string()
    } else {
        options
            .iter()
            .map(|(tag, label)| format!("- {tag}：例如「{label}」"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let recent = if recent.is_empty() {
        "（还没有）".to_string()
    } else {
        recent
            .iter()
            .map(|t| format!("- {t}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "请给这个角色安排「今天想怎么过」。\n\
         可选的活动标签：\n{list}\n\
         最近几天的主题（今天要换一个不一样的）：\n{recent}\n\
         只输出一行 JSON，不要代码块、不要解释、不要换行：\n\
         {{\"theme\":\"今天的一句话主题（{} 个字以内）\",\"say\":\"角色自己说的一句话（{} 个字以内）\",\"focus\":[\"今天想多做的标签\"],\"avoid\":[\"今天想少做的标签\"]}}\n\
         标签只能从上面列表里选，focus 和 avoid 加起来最多 {} 个，两者不要选同一个标签；没有合适的就留空数组。",
        THEME_MAX_UNITS as u32, SAY_MAX_UNITS as u32, MAX_TAGS
    )
}

/// 从模型输出里抠出第一个 JSON 对象（容忍 ```json 代码块与前后解释）
fn extract_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

/// 标签白名单过滤：只留角色身上真有的标签、去重、丢掉与 `exclude` 重复的、最多 MAX_TAGS 个
fn pick_tags(raw: &[String], allowed: &BTreeSet<String>, exclude: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in raw {
        let tag = tag.trim();
        if tag.is_empty() || !allowed.contains(tag) || exclude.iter().any(|e| e == tag) {
            continue;
        }
        if !out.iter().any(|t| t == tag) {
            out.push(tag.to_string());
        }
        if out.len() >= MAX_TAGS {
            break;
        }
    }
    out
}

/// 校验并规整模型输出：主题/台词非空且在长度内、标签在白名单内、focus 与 avoid 不重叠
fn validate(mut plan: DailyPlan, allowed: &BTreeSet<String>) -> Result<DailyPlan, String> {
    plan.theme = plan.theme.trim().to_string();
    plan.say = plan.say.trim().to_string();
    if plan.theme.is_empty() {
        return Err("theme 是空的".into());
    }
    if visual_width(&plan.theme) > THEME_MAX_UNITS {
        return Err(format!("theme 超过了 {THEME_MAX_UNITS} 个单位"));
    }
    if plan.say.is_empty() {
        return Err("say 是空的".into());
    }
    if visual_width(&plan.say) > SAY_MAX_UNITS {
        return Err(format!("say 超过了 {SAY_MAX_UNITS} 个单位"));
    }
    plan.focus = pick_tags(&plan.focus, allowed, &[]);
    plan.avoid = pick_tags(&plan.avoid, allowed, &plan.focus);
    Ok(plan)
}

/// 解析模型输出：抠 JSON → 反序列化 → 校验；失败返回原因（供重试提示）
fn parse_plan(raw: &str, allowed: &BTreeSet<String>) -> Result<DailyPlan, String> {
    let object = extract_object(raw).ok_or("没有找到 JSON 对象")?;
    let plan: DailyPlan = serde_json::from_str(object).map_err(|e| e.to_string())?;
    validate(plan, allowed)
}

/// 生成当日计划：一次调用 + 最多一次带反馈的重试
async fn generate(
    cfg: &crate::llm::LlmConfig,
    persona: &PersonaConfig,
    recent: &[String],
) -> Option<DailyPlan> {
    let options = tag_options(persona);
    let allowed: BTreeSet<String> = options.keys().cloned().collect();
    let mut messages = vec![
        LlmMessage {
            role: "system".into(),
            content: build_system_prompt(persona).into(),
        },
        LlmMessage {
            role: "user".into(),
            content: build_user_prompt(&options, recent).into(),
        },
    ];
    for _ in 0..2 {
        let raw = crate::llm::chat_completion_stream(cfg, &cfg.model, &messages, |_| {})
            .await
            .ok()?;
        match parse_plan(&raw, &allowed) {
            Ok(plan) => return Some(plan),
            Err(reason) => {
                messages.push(LlmMessage {
                    role: "assistant".into(),
                    content: raw.into(),
                });
                messages.push(LlmMessage {
                    role: "user".into(),
                    content: format!(
                        "上一次的输出不能用作安排（{reason}）。请只输出一行符合要求的 JSON。"
                    )
                    .into(),
                });
            }
        }
    }
    None
}

/// 缓存文件路径：{app_config}/plans/{persona_id}.json
fn cache_file_path(app: &AppHandle, persona_id: &str) -> Result<std::path::PathBuf, String> {
    if !crate::engine::is_valid_history_id(persona_id) {
        return Err(format!("非法角色 id：{persona_id}"));
    }
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("plans").join(format!("{persona_id}.json")))
}

/// 读取缓存；任何失败返回默认空缓存（回退到"没有计划"，不报错）
pub fn load(app: &AppHandle, persona_id: &str) -> PlanCache {
    let Ok(path) = cache_file_path(app, persona_id) else {
        return PlanCache::default();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return PlanCache::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 保存缓存（原子写）
pub(crate) fn save(app: &AppHandle, persona_id: &str, cache: &PlanCache) -> Result<(), String> {
    let path = cache_file_path(app, persona_id)?;
    let json = serde_json::to_string_pretty(cache).map_err(|e| e.to_string())?;
    crate::util::atomic_write(&path, &json)
}

/// 今天的计划：非当日 / 今天还没有 / 今天已失败 → None
pub fn today_plan(state: &Mutex<PlanState>) -> Option<DailyPlan> {
    let s = crate::util::lock(state);
    if s.cache.date != today_str() || s.cache.failed {
        return None;
    }
    s.cache.plan.clone()
}

/// 今天的计划是否需要生成（还没有，且今天也没失败过）
pub fn needs_generation(state: &Mutex<PlanState>) -> bool {
    let s = crate::util::lock(state);
    if s.cache.date != today_str() {
        return true;
    }
    s.cache.plan.is_none() && !s.cache.failed
}

/// 触发入口：开关开、今天还没有计划、已配置 Key、无并发任务时，后台生成一次。
/// 幂等，可在启动/切角色/跨天时随意调用。
pub fn maybe_spawn_for_today(app: &AppHandle) {
    let engine = app.state::<StateEngine>();
    if !engine.plan_needs_today() {
        log::debug!("每日计划：当天已有计划或已失败过，跳过");
        return;
    }
    if !engine.prefs().ai_bubbles {
        log::debug!("每日计划：总开关关闭，跳过");
        return;
    }
    if engine.plan_generating.load(Ordering::Relaxed) {
        return;
    }
    if crate::llm::load_config(app).api_key.is_empty() {
        log::debug!("每日计划：未配置大模型 Key，跳过");
        return;
    }
    if engine
        .plan_generating
        .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        // Drop 兜底：即使任务 panic 也把标志复位，避免当天再也不生成
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Relaxed);
            }
        }
        let engine = app2.state::<StateEngine>();
        let _reset = Reset(&engine.plan_generating);
        let cfg = crate::llm::load_config(&app2);
        if cfg.api_key.is_empty() {
            return;
        }
        let persona = engine.persona();
        let persona_id = persona.id.clone();
        let recent = engine.plan_recent_themes();
        match generate(&cfg, &persona, &recent).await {
            // 生成期间换了角色 → 丢弃（这份计划属于旧角色）
            Some(plan) if engine.persona_id() == persona_id => {
                log::info!(
                    "每日计划：{persona_id} 主题「{}」，焦点 {:?} / 回避 {:?}，开口说「{}」",
                    plan.theme,
                    plan.focus,
                    plan.avoid,
                    plan.say
                );
                engine.apply_plan(&persona_id, plan)
            }
            Some(_) => {}
            None => {
                log::warn!("每日计划：{persona_id} 生成失败，当天按默认编排");
                engine.record_plan_failure(&persona_id)
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link_persona() -> PersonaConfig {
        serde_json::from_str(include_str!("../../resources/characters/link/persona.json")).unwrap()
    }

    #[test]
    fn tag_options_come_from_chains() {
        let persona = link_persona();
        let options = tag_options(&persona);
        // 内置林克：链上打了 explore / social / calm / rest
        for tag in ["explore", "social", "calm", "rest"] {
            assert!(options.contains_key(tag), "缺少标签 {tag}");
        }
        // 例子取自真实链名（同一标签可能挂多条链，取哪条都行），模型才知道这个标签意味着什么
        let social_labels: Vec<&str> = persona
            .states
            .values()
            .flat_map(|cfg| cfg.chains.iter())
            .filter(|chain| chain.tags.iter().any(|t| t == "social"))
            .map(|chain| chain.label.as_str())
            .collect();
        assert!(
            social_labels.contains(&options["social"].as_str()),
            "{options:?}"
        );
    }

    #[test]
    fn prompt_asks_for_json_and_lists_tags() {
        let persona = link_persona();
        let system = build_system_prompt(&persona);
        assert!(system.contains(&persona.system_prompt.definition), "{system}");
        let user = build_user_prompt(&tag_options(&persona), &["昨天在忙保养".to_string()]);
        for key in ["theme", "say", "focus", "avoid"] {
            assert!(user.contains(key), "提示词缺少字段 {key}：{user}");
        }
        assert!(user.contains("- explore"), "{user}");
        // 近期主题要带上，要求今天换一个
        assert!(user.contains("昨天在忙保养"), "{user}");
    }

    #[test]
    fn parse_accepts_fenced_json_and_filters_tags() {
        let allowed: BTreeSet<String> = ["explore", "rest"].iter().map(|s| s.to_string()).collect();
        let raw = "```json\n{\"theme\":\"今天出门看看\",\"say\":\"今天想去外面走走。\",\
                   \"focus\":[\"explore\",\"不存在\",\"explore\"],\"avoid\":[\"explore\",\"rest\"]}\n```";
        let plan = parse_plan(raw, &allowed).unwrap();
        assert_eq!(plan.theme, "今天出门看看");
        // 白名单外的标签被丢掉、重复的标签去重、avoid 与 focus 不重叠
        assert_eq!(plan.focus, vec!["explore"]);
        assert_eq!(plan.avoid, vec!["rest"]);
    }

    #[test]
    fn parse_rejects_empty_or_overlong() {
        let allowed: BTreeSet<String> = ["explore"].iter().map(|s| s.to_string()).collect();
        assert!(parse_plan("这里没有 JSON", &allowed).is_err());
        let empty = "{\"theme\":\"\",\"say\":\"你好\"}";
        assert!(parse_plan(empty, &allowed).is_err());
        let long_say = format!("{{\"theme\":\"主题\",\"say\":\"{}\"}}", "好".repeat(40));
        assert!(parse_plan(&long_say, &allowed).is_err());
    }

    #[test]
    fn cache_round_trips_and_tolerates_missing_fields() {
        let cache = PlanCache {
            date: "2026-09-14".into(),
            failed: false,
            plan: Some(DailyPlan {
                theme: "今天静一静".into(),
                say: "今天想安静一会儿。".into(),
                focus: vec!["calm".into()],
                avoid: vec![],
            }),
            recent: vec!["昨天忙着出门".into()],
        };
        let json = serde_json::to_string(&cache).unwrap();
        let back: PlanCache = serde_json::from_str(&json).unwrap();
        assert_eq!(back.plan.unwrap().focus, vec!["calm".to_string()]);
        assert_eq!(back.recent.len(), 1);
        // 旧文件缺字段：serde default 兜底
        let old: PlanCache = serde_json::from_str("{}").unwrap();
        assert!(old.plan.is_none() && old.recent.is_empty() && !old.failed);
    }
}
