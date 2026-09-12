//! 播放调度：由 Rust 决定「状态内走哪条链、当前播哪一段的哪个动作实例」，前端只按事件播放。
//!
//! 层级：状态（固定 6 个）→ 链（链内有序、链间可选）→ 段（有起止的 UI 表现）→ 动作实例（clip）。
//! 台词挂在动作上、且只在动作实例内出现，保证"说的"和"演的"严格一致。

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Local};
use serde::Serialize;

use crate::engine::{refill_bag, AnimationClipConfig, ChainConfig, PersonaConfig, SceneConfig};

/// 单个动作实例的时长上限（毫秒）：防止手改配置把角色卡在同一个动作上
pub const MAX_ACTION_MS: i64 = 5 * 60 * 1000;
/// 旧格式 loops 的上限（避免旧配置异常放大）
pub const MAX_STEP_LOOPS: u32 = 20;

/// 发给前端的播放指令（前端只负责按它渲染，不再自己挑场景）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaybackEvent {
    pub state: String,
    /// 链 id / 中文说明（调试与后续 UI 用）
    pub chain_id: String,
    pub chain_label: String,
    /// 段 id / 中文说明；沿用 `scene_*` 字段名，前端无需改动
    pub scene_id: String,
    pub scene_label: String,
    /// 段内的第几步（0 起）
    pub step_index: u32,
    /// 动作 id：前端 dataset 与气泡池都用它
    pub clip: String,
    /// 帧条资源路径（站内路径或磁盘绝对路径）
    pub spritesheet: String,
    pub frames: u32,
    pub frame_ms: u64,
    pub loops: u32,
    /// 动作实例时长 = 目标秒数（取整到整数个循环）或 loops × 原生时长
    pub duration_ms: u64,
}

/// 当前正在播放的动作实例
#[derive(Debug, Clone)]
pub(crate) struct Current {
    pub(crate) state: String,
    pub(crate) chain_id: String,
    pub(crate) scene_id: String,
    pub(crate) scene_label: String,
    pub(crate) step_index: usize,
    pub(crate) clip: String,
    pub(crate) started_at: DateTime<Local>,
    pub(crate) next_at: DateTime<Local>,
    pub(crate) duration_ms: u64,
}

/// 播放调度状态（与状态引擎同生命周期，不持久化）
#[derive(Debug, Default)]
pub struct Playback {
    current: Option<Current>,
    /// state -> 链 id 的加权洗牌袋：一轮内每条链按权重出现；链内顺序不受影响
    bags: HashMap<String, VecDeque<String>>,
    /// state -> 上一轮最后播放的链（跨轮防连续重复）
    last_chain: HashMap<String, String>,
}

impl Playback {
    /// 切换角色时清空：袋子与"当前播放"都属于旧角色
    pub fn clear(&mut self) {
        self.current = None;
        self.bags.clear();
        self.last_chain.clear();
    }

    /// 为指定状态选一条链，并从第一段第一个动作开始（状态切换 / 启动 / 切角色后调用）
    pub fn reset(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        self.current = None;
        let chain = self.pick_chain(persona, state)?;
        self.start_chain(persona, state, chain, now)
    }

    /// 到点就推进：同段下一步 → 同链下一段 → 换一条链
    pub fn advance(
        &mut self,
        persona: &PersonaConfig,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let current = self.current.clone()?;
        if now < current.next_at {
            return None;
        }
        // 1) 同一段还有下一步
        if let Some(chain) = find_chain(persona, &current.state, &current.chain_id) {
            if let Some(segment) = find_segment(persona, &current.state, &current.scene_id) {
                if current.step_index + 1 < segment.steps.len() {
                    if let Some(event) = self.start_step(
                        persona,
                        &current.state,
                        chain,
                        segment,
                        current.step_index + 1,
                        now,
                    ) {
                        return Some(event);
                    }
                }
            }
        }
        // 2) 同一条链还有下一段（跳过没有有效动作的段）
        if let Some(chain) = find_chain(persona, &current.state, &current.chain_id) {
            if let Some(pos) = chain.segments.iter().position(|id| id == &current.scene_id) {
                for segment_id in chain.segments.iter().skip(pos + 1) {
                    if let Some(segment) = find_segment(persona, &current.state, segment_id) {
                        if let Some(event) =
                            self.start_step(persona, &current.state, chain, segment, 0, now)
                        {
                            return Some(event);
                        }
                    }
                }
            }
        }
        // 3) 链走完 → 按权重换一条链
        let chain = self.pick_chain(persona, &current.state)?;
        self.start_chain(persona, &current.state, chain, now)
    }

    /// 下一次需要推进的时刻（并入引擎节拍线程的唤醒计算）
    pub fn next_at(&self) -> Option<DateTime<Local>> {
        self.current.as_ref().map(|current| current.next_at)
    }

    /// 是否到了推进时刻。引擎先用它判断，没到点就直接返回。
    pub fn is_due(&self, now: DateTime<Local>) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| now >= current.next_at)
    }

    /// 当前在播的动作 id
    pub fn current_clip(&self) -> Option<&str> {
        self.current.as_ref().map(|current| current.clip.as_str())
    }

    /// 当前动作实例的完整快照（对话 system prompt 需要动作说明）
    pub(crate) fn current(&self) -> Option<&Current> {
        self.current.as_ref()
    }

    /// 当前动作实例还剩多少毫秒
    pub fn remaining_ms(&self, now: DateTime<Local>) -> Option<i64> {
        self.current
            .as_ref()
            .map(|current| (current.next_at - now).num_milliseconds())
    }

    /// 从状态内的链里按权重抽一条（洗牌袋；一轮内不重复，跨轮不连续重复）。
    /// 链内段的顺序由作者定，绝不打乱——因果链靠这个保证。
    fn pick_chain<'a>(
        &mut self,
        persona: &'a PersonaConfig,
        state: &str,
    ) -> Option<&'a ChainConfig> {
        let chains = valid_chains(persona, state);
        if chains.is_empty() {
            return None;
        }
        if self.bags.get(state).is_none_or(|bag| bag.is_empty()) {
            let mut pool: Vec<String> = Vec::new();
            for chain in &chains {
                for _ in 0..chain.weight.max(1) {
                    pool.push(chain.id.clone());
                }
            }
            let last = self.last_chain.get(state).cloned();
            self.bags
                .insert(state.to_string(), refill_bag(pool, last.as_deref()));
        }
        let last = self.last_chain.get(state).cloned();
        let bag = self.bags.get_mut(state)?;
        let index = bag
            .iter()
            .position(|id| Some(id) != last.as_ref())
            .unwrap_or(0);
        let id = bag.remove(index)?;
        // 袋里残留的失效链 id 直接丢弃后重挑
        let picked = match chains.iter().find(|chain| chain.id == id) {
            Some(chain) => *chain,
            None => return self.pick_chain(persona, state),
        };
        self.last_chain.insert(state.to_string(), id);
        Some(picked)
    }

    /// 从一条链的第一段开始播（跳过没有有效动作的段）
    fn start_chain(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        chain: &ChainConfig,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        for segment_id in &chain.segments {
            let Some(segment) = find_segment(persona, state, segment_id) else {
                continue;
            };
            if let Some(event) = self.start_step(persona, state, chain, segment, 0, now) {
                return Some(event);
            }
        }
        None
    }

    /// 开播某一步（一个动作实例），时长按 `seconds` 目标取整到整数个循环
    fn start_step(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        chain: &ChainConfig,
        scene: &SceneConfig,
        index: usize,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let step = scene.steps.get(index)?;
        let clip: &AnimationClipConfig = persona.clips.get(&step.clip)?;
        let frames = clip.frames.max(1);
        let frame_ms = clip.frame_ms.max(1);
        let native_ms = (frames as i64 * frame_ms as i64).max(1);
        let max_loops = ((MAX_ACTION_MS / native_ms).max(1)) as u32;
        let (loops, duration_ms) = match step.seconds {
            // 新格式：目标秒数 → 向上取整到整数个循环，保证动作实例至少播这么久
            Some(seconds) => {
                let target_ms = (seconds.max(1) as i64) * 1000;
                let loops =
                    ((target_ms + native_ms - 1) / native_ms).clamp(1, max_loops as i64) as u32;
                (loops, loops as i64 * native_ms)
            }
            // 旧格式：按 loops 播放
            None => {
                let loops = step.loops.clamp(1, MAX_STEP_LOOPS).min(max_loops);
                (loops, loops as i64 * native_ms)
            }
        };
        let duration_ms = duration_ms.max(1) as u64;
        self.current = Some(Current {
            state: state.to_string(),
            chain_id: chain.id.clone(),
            scene_id: scene.id.clone(),
            scene_label: scene.label.clone(),
            step_index: index,
            clip: step.clip.clone(),
            started_at: now,
            next_at: now + chrono::Duration::milliseconds(duration_ms as i64),
            duration_ms,
        });
        Some(PlaybackEvent {
            state: state.to_string(),
            chain_id: chain.id.clone(),
            chain_label: chain.label.clone(),
            scene_id: scene.id.clone(),
            scene_label: scene.label.clone(),
            step_index: index as u32,
            clip: step.clip.clone(),
            spritesheet: clip.spritesheet.clone(),
            frames,
            frame_ms,
            loops,
            duration_ms,
        })
    }
}

fn find_segment<'a>(
    persona: &'a PersonaConfig,
    state: &str,
    segment_id: &str,
) -> Option<&'a SceneConfig> {
    persona
        .scenes
        .get(state)?
        .iter()
        .find(|scene| scene.id == segment_id)
}

fn find_chain<'a>(
    persona: &'a PersonaConfig,
    state: &str,
    chain_id: &str,
) -> Option<&'a ChainConfig> {
    persona
        .chains
        .get(state)?
        .iter()
        .find(|chain| chain.id == chain_id)
}

/// 某状态下可用的链：至少有一个段包含已定义动作
fn valid_chains<'a>(persona: &'a PersonaConfig, state: &str) -> Vec<&'a ChainConfig> {
    persona
        .chains
        .get(state)
        .map(|chains| {
            chains
                .iter()
                .filter(|chain| {
                    chain.segments.iter().any(|segment_id| {
                        find_segment(persona, state, segment_id).is_some_and(|scene| {
                            scene
                                .steps
                                .iter()
                                .any(|step| persona.clips.contains_key(&step.clip))
                        })
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{SceneStepConfig, ScheduleConfig, StateConfig, SystemPromptConfig};
    use chrono::TimeZone;
    use std::collections::HashMap;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        let date = Local::now().date_naive();
        Local
            .from_local_datetime(&date.and_hms_opt(h, m, s).unwrap())
            .single()
            .unwrap()
    }

    fn clip(frames: u32, frame_ms: u64) -> AnimationClipConfig {
        AnimationClipConfig {
            spritesheet: "/characters/link/clips/x.webp".into(),
            label: "测试动作".into(),
            description: "角色正在做测试动作".into(),
            frames,
            frame_ms,
            bubbles: vec![],
        }
    }

    /// 一个状态两条链：
    /// - chain_ab（a → b）：a 单步（200ms 原生，目标 1 秒）；b 两步（先 loops=2，再目标 1 秒）
    /// - chain_b：只有 b
    fn persona() -> PersonaConfig {
        let mut clips = HashMap::new();
        clips.insert("one".to_string(), clip(2, 100)); // 原生 200ms
        clips.insert("two".to_string(), clip(3, 100)); // 原生 300ms
        clips.insert("three".to_string(), clip(2, 100)); // 原生 200ms
        let mut scenes = HashMap::new();
        scenes.insert(
            "routine".to_string(),
            vec![
                SceneConfig {
                    id: "a".into(),
                    label: "甲".into(),
                    weight: 1,
                    steps: vec![SceneStepConfig {
                        clip: "one".into(),
                        seconds: Some(1),
                        loops: 1,
                    }],
                },
                SceneConfig {
                    id: "b".into(),
                    label: "乙".into(),
                    weight: 1,
                    steps: vec![
                        SceneStepConfig {
                            clip: "two".into(),
                            seconds: None,
                            loops: 2,
                        },
                        SceneStepConfig {
                            clip: "three".into(),
                            seconds: Some(1),
                            loops: 1,
                        },
                    ],
                },
            ],
        );
        let mut chains = HashMap::new();
        chains.insert(
            "routine".to_string(),
            vec![
                ChainConfig {
                    id: "chain_ab".into(),
                    label: "甲→乙".into(),
                    weight: 1,
                    segments: vec!["a".into(), "b".into()],
                },
                ChainConfig {
                    id: "chain_b".into(),
                    label: "乙".into(),
                    weight: 1,
                    segments: vec!["b".into()],
                },
            ],
        );
        PersonaConfig {
            id: "test".into(),
            name: "测试".into(),
            display_w: 10,
            display_h: 10,
            system_prompt: SystemPromptConfig {
                definition: "d".into(),
                reply_style: "r".into(),
            },
            states: HashMap::from([(
                "routine".to_string(),
                StateConfig {
                    label: "日常".into(),
                    talkativeness: String::new(),
                    tone: String::new(),
                    bubble_gap_min: None,
                },
            )]),
            clips,
            scenes,
            chains,
            schedule: ScheduleConfig {
                r#loop: vec![],
                time: vec![],
            },
        }
    }

    /// 单链版本，用于确定性地验证链内顺序
    fn single_chain_persona() -> PersonaConfig {
        let mut p = persona();
        p.chains
            .insert("routine".to_string(), vec![p.chains["routine"][0].clone()]);
        p
    }

    #[test]
    fn seconds_round_up_to_whole_loops() {
        let p = single_chain_persona();
        let mut playback = Playback::default();
        let now = at(10, 0, 0);
        let event = playback.reset(&p, "routine", now).unwrap();
        assert_eq!(event.scene_id, "a");
        assert_eq!(event.clip, "one");
        // 目标 1 秒、原生 200ms → 5 个循环
        assert_eq!(event.loops, 5);
        assert_eq!(event.duration_ms, 1000);
        assert_eq!(playback.next_at(), Some(now + chrono::Duration::seconds(1)));
    }

    #[test]
    fn advance_follows_chain_and_segment_steps() {
        let p = single_chain_persona();
        let mut playback = Playback::default();
        let start = at(10, 0, 0);
        let first = playback.reset(&p, "routine", start).unwrap();
        assert_eq!(first.scene_id, "a");
        assert_eq!(first.chain_id, "chain_ab");

        // a 播完（1s）→ 同链下一段 b 的第一步（旧格式 loops=2 → 600ms）
        let second = playback
            .advance(&p, start + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(second.scene_id, "b");
        assert_eq!(second.step_index, 0);
        assert_eq!(second.clip, "two");
        assert_eq!(second.duration_ms, 600);

        // b 的第一步播完 → 同段第二步（目标 1s）
        let third = playback
            .advance(&p, start + chrono::Duration::milliseconds(1600))
            .unwrap();
        assert_eq!(third.scene_id, "b");
        assert_eq!(third.step_index, 1);
        assert_eq!(third.clip, "three");
        assert_eq!(third.duration_ms, 1000);

        // b 播完 → 链走完 → 重新开始（单链只能回到 a）
        let fourth = playback
            .advance(&p, start + chrono::Duration::milliseconds(2600))
            .unwrap();
        assert_eq!(fourth.scene_id, "a");
    }

    #[test]
    fn legacy_loops_still_work() {
        let p = persona();
        let mut playback = Playback::default();
        let now = at(10, 0, 0);
        let mut p2 = p.clone();
        p2.chains.insert(
            "routine".to_string(),
            vec![ChainConfig {
                id: "only_b".into(),
                label: "乙".into(),
                weight: 1,
                segments: vec!["b".into()],
            }],
        );
        let event = playback.reset(&p2, "routine", now).unwrap();
        assert_eq!(event.clip, "two");
        assert_eq!(event.loops, 2);
        assert_eq!(event.duration_ms, 600);
    }

    #[test]
    fn chain_selection_never_repeats_back_to_back() {
        let p = persona();
        let mut playback = Playback::default();
        let mut previous = String::new();
        for minute in 0..10 {
            let event = playback.reset(&p, "routine", at(10, minute, 0)).unwrap();
            assert_ne!(event.chain_id, previous, "连续两次抽到同一条链");
            previous = event.chain_id;
        }
    }

    #[test]
    fn clear_drops_current_and_bags() {
        let p = persona();
        let mut playback = Playback::default();
        playback.reset(&p, "routine", at(10, 0, 0));
        playback.clear();
        assert!(playback.current_clip().is_none());
        assert!(playback.next_at().is_none());
    }
}
