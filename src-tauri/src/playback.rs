//! 播放调度：由 Rust 决定「状态内走哪条链、当前播哪一段的哪个动作实例」，前端只按事件播放。
//!
//! 层级：状态（固定 6 个）→ 链（链内有序、链间可选）→ 段（有起止的 UI 表现）→ 动作实例（clip）。
//! 台词挂在动作上、且只在动作实例内出现，保证"说的"和"演的"严格一致。

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Local};
use serde::Serialize;

use crate::engine::{
    refill_bag, AnimationClipConfig, ChainConfig, PersonaConfig, SceneConfig, SceneStepConfig,
};

/// 单个动作实例的时长上限（毫秒）：防止手改配置把角色卡在同一个动作上
pub const MAX_ACTION_MS: i64 = 5 * 60 * 1000;
/// 旧格式 loops 的上限（避免旧配置异常放大）
pub const MAX_STEP_LOOPS: u32 = 20;
/// "注意你"实例在事件里的链/段标识与标题
const ACK_CHAIN_ID: &str = "__ack__";
const ACK_SEGMENT_ID: &str = "__ack__";
const ACK_LABEL: &str = "注意到你";

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
    /// 是否是「注意到你」这类短反应实例：反应很短，说话规则要放宽（见 engine::ack_speech_window）
    pub(crate) ack: bool,
}

/// 播放调度状态（与状态引擎同生命周期，不持久化）
#[derive(Debug, Default)]
pub struct Playback {
    current: Option<Current>,
    /// 被打断的动作：播完"注意你"后回到它继续（只保留一层，够用且不会无限套娃）
    suspended: Option<Current>,
    /// 正在播的"注意你"步骤；非空表示处于打断反应中
    ack_steps: Vec<SceneStepConfig>,
    ack_index: usize,
    /// state -> 链 id 的加权洗牌袋：一轮内每条链按权重出现；链内顺序不受影响
    bags: HashMap<String, VecDeque<String>>,
    /// state -> 上一轮最后播放的链（跨轮防连续重复）
    last_chain: HashMap<String, String>,
}

impl Playback {
    /// 切换角色时清空：袋子与"当前播放"都属于旧角色
    pub fn clear(&mut self) {
        self.current = None;
        self.suspended = None;
        self.ack_steps.clear();
        self.ack_index = 0;
        self.bags.clear();
        self.last_chain.clear();
    }

    /// 是否正处于"被打断去注意用户"的状态
    pub fn is_acknowledging(&self) -> bool {
        !self.ack_steps.is_empty()
    }

    /// 为指定状态选一条链，并从第一段第一个动作开始（状态切换 / 启动 / 切角色后调用）
    pub fn reset(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        self.current = None;
        // 换状态/换角色时，被打断的旧活动不再恢复
        self.suspended = None;
        self.ack_steps.clear();
        self.ack_index = 0;
        let chain = self.pick_chain(persona, state)?;
        self.start_chain(persona, state, chain, now)
    }

    /// 「被用户注意到」：挂起当前动作，插一段短反应（注意你），之后再回到原处继续。
    ///
    /// 返回要播的反应动作；以下情况不打断并返回 None：
    /// - 没有正在播的内容 / 已经在反应中；
    /// - 当前动作剩余不足 3 秒（刚打断就结束，更突兀）；
    /// - 反应步骤里没有任何可用动作。
    pub fn acknowledge(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        steps: &[SceneStepConfig],
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        if self.is_acknowledging() {
            return None;
        }
        let current = self.current.clone()?;
        let min_remaining_ms = persona.acknowledge.min_remaining_s.max(0) * 1000;
        if (current.next_at - now).num_milliseconds() < min_remaining_ms {
            return None;
        }
        self.suspended = Some(current);
        self.ack_steps = steps.to_vec();
        self.ack_index = 0;
        self.start_ack_step(persona, state, now)
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
        // 0) 正在播「注意你」：还有下一步就继续，播完则回到被打断的活动
        if self.is_acknowledging() {
            if let Some(next) = self.next_ack_index(persona) {
                self.ack_index = next;
                return self.start_ack_step(persona, &current.state, now);
            }
            return self.resume_suspended(persona, now);
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

    /// 播「注意你」反应里的第 `ack_index` 步（跳过引用不到动作的步骤）
    fn start_ack_step(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let step = self.ack_steps.get(self.ack_index)?.clone();
        let chain = ChainConfig {
            id: ACK_CHAIN_ID.into(),
            label: ACK_LABEL.into(),
            weight: 1,
            segments: vec![ACK_SEGMENT_ID.into()],
        };
        let segment = SceneConfig {
            id: ACK_SEGMENT_ID.into(),
            label: ACK_LABEL.into(),
            weight: 1,
            steps: vec![step],
        };
        self.start_step_with_ack(persona, state, &chain, &segment, 0, now, true)
    }

    /// 反应步骤里下一个有可用动作的下标
    fn next_ack_index(&self, persona: &PersonaConfig) -> Option<usize> {
        (self.ack_index + 1..self.ack_steps.len())
            .find(|index| persona.clips.contains_key(&self.ack_steps[*index].clip))
    }

    /// 反应播完：回到被打断的那一步重新开始；配置已不可用时按常规换链
    fn resume_suspended(
        &mut self,
        persona: &PersonaConfig,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        self.ack_steps.clear();
        self.ack_index = 0;
        let Some(suspended) = self.suspended.take() else {
            let state = self.current.as_ref()?.state.clone();
            let chain = self.pick_chain(persona, &state)?;
            return self.start_chain(persona, &state, chain, now);
        };
        let chain = find_chain(persona, &suspended.state, &suspended.chain_id);
        let segment = find_segment(persona, &suspended.state, &suspended.scene_id);
        match (chain, segment) {
            (Some(chain), Some(segment)) => self.start_step(
                persona,
                &suspended.state,
                chain,
                segment,
                suspended.step_index,
                now,
            ),
            _ => {
                let chain = self.pick_chain(persona, &suspended.state)?;
                self.start_chain(persona, &suspended.state, chain, now)
            }
        }
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
        self.start_step_with_ack(persona, state, chain, scene, index, now, false)
    }

    /// 开播某一步（一个动作实例），时长按 `seconds` 目标取整到整数个循环。
    /// `ack` 标记这是"注意到你"的短反应。
    #[allow(clippy::too_many_arguments)]
    fn start_step_with_ack(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        chain: &ChainConfig,
        scene: &SceneConfig,
        index: usize,
        now: DateTime<Local>,
        ack: bool,
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
            ack,
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
            // 默认给所有状态配一个 1 秒的"注意你"反应，供 acknowledge 用例使用
            acknowledge: crate::engine::AcknowledgeConfig {
                default: vec![SceneStepConfig {
                    clip: "three".into(),
                    seconds: Some(1),
                    loops: 1,
                }],
                ..Default::default()
            },
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

    /// 长动作实例版本：「注意你」的打断/恢复用例需要当前动作还剩足够时间
    fn long_instance_persona() -> PersonaConfig {
        let mut p = single_chain_persona();
        for scene in p.scenes.get_mut("routine").unwrap() {
            for step in &mut scene.steps {
                step.seconds = Some(30);
            }
        }
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
        assert!(!playback.is_acknowledging());
    }

    #[test]
    fn acknowledge_interrupts_then_resumes_same_action() {
        let p = long_instance_persona();
        let mut playback = Playback::default();
        let start = at(10, 0, 0);
        let before = playback.reset(&p, "routine", start).unwrap();
        let steps = p.acknowledge.steps_for("routine", "click").unwrap().to_vec();

        let ack = playback
            .acknowledge(&p, "routine", &steps, start + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(ack.clip, "three");
        assert_eq!(ack.scene_label, ACK_LABEL);
        assert!(playback.is_acknowledging());
        assert_eq!(playback.current_clip(), Some("three"));

        // 反应播完 → 回到被打断的那条链、那一段、那一步
        let resumed_at =
            start + chrono::Duration::seconds(1) + chrono::Duration::milliseconds(ack.duration_ms as i64);
        let resumed = playback.advance(&p, resumed_at).unwrap();
        assert_eq!(resumed.clip, before.clip);
        assert_eq!(resumed.chain_id, before.chain_id);
        assert_eq!(resumed.scene_id, before.scene_id);
        assert_eq!(resumed.step_index, before.step_index);
        assert!(!playback.is_acknowledging(), "恢复后应退出反应状态");
    }

    #[test]
    fn acknowledge_skips_when_no_current_already_reacting_or_too_late() {
        let p = long_instance_persona();
        let steps = p.acknowledge.steps_for("routine", "click").unwrap().to_vec();

        // 没有正在播的内容 → 不打断
        let mut idle = Playback::default();
        assert!(idle
            .acknowledge(&p, "routine", &steps, at(10, 0, 0))
            .is_none());

        let mut playback = Playback::default();
        let start = at(10, 0, 0);
        playback.reset(&p, "routine", start).unwrap();
        let ack = playback
            .acknowledge(&p, "routine", &steps, start + chrono::Duration::seconds(1))
            .unwrap();
        // 反应中不重复打断
        assert!(playback
            .acknowledge(&p, "routine", &steps, start + chrono::Duration::seconds(2))
            .is_none());

        // 恢复后，当前动作只剩不到 3 秒 → 不再打断
        let resumed_at = start
            + chrono::Duration::seconds(1)
            + chrono::Duration::milliseconds(ack.duration_ms as i64);
        playback.advance(&p, resumed_at).unwrap();
        let end = playback.current.as_ref().unwrap().next_at;
        assert!(playback
            .acknowledge(&p, "routine", &steps, end - chrono::Duration::seconds(1))
            .is_none());
    }
}
