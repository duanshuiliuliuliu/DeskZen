//! 场景播放调度：由 Rust 决定「接下来播哪个场景的哪个动作」，前端只按事件播放。
//!
//! 这样「画面」「气泡文案」「播放时长」共用同一个事实来源——后端始终知道当前在播
//! 哪个动作，就能保证气泡说的是眼里正在发生的事。

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Local};
use serde::Serialize;

use crate::engine::{refill_bag, AnimationClipConfig, PersonaConfig, SceneConfig};

/// 单个动作的循环次数上限：防止手改配置把角色卡在同一个动作上
pub const MAX_STEP_LOOPS: u32 = 20;
/// 一个场景至少持续多久（毫秒）：太短会让角色像"坐不住"，每隔几秒就换一件事做。
/// 场景走完一遍后，若整体还不到这个时长就再走一遍（同一动作自然循环，不会看起来被打断）。
const MIN_SCENE_HOLD_MS: i64 = 12_000;

/// 发给前端的播放指令（前端只负责按它渲染，不再自己挑场景）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaybackEvent {
    /// 语义状态 id
    pub state: String,
    /// 场景 id / 中文说明（标题用）
    pub scene_id: String,
    pub scene_label: String,
    /// 场景内的第几步（0 起）
    pub step_index: u32,
    /// 动作 id：前端 dataset 与气泡池都用它
    pub clip: String,
    /// 帧条资源路径（站内路径或磁盘绝对路径）
    pub spritesheet: String,
    pub frames: u32,
    pub frame_ms: u64,
    pub loops: u32,
    /// 本步总时长 = frames × frame_ms × loops
    pub duration_ms: u64,
}

/// 当前正在播放的一步
#[derive(Debug, Clone)]
pub(crate) struct Current {
    pub(crate) state: String,
    pub(crate) scene_id: String,
    pub(crate) scene_label: String,
    /// 本场景第一次开播的时刻（用于「同一场景至少播 N 秒」）
    pub(crate) scene_started_at: DateTime<Local>,
    pub(crate) step_index: usize,
    pub(crate) clip: String,
    pub(crate) next_at: DateTime<Local>,
}

/// 场景调度状态（与状态引擎同生命周期，不持久化）
#[derive(Debug, Default)]
pub struct Playback {
    current: Option<Current>,
    /// state -> 场景 id 的加权洗牌袋：一轮内每个场景按权重出现，不会一直抽不到某个场景
    bags: HashMap<String, VecDeque<String>>,
    /// state -> 上一轮最后播放的场景（跨轮防连续重复）
    last_scene: HashMap<String, String>,
}

impl Playback {
    /// 切换角色时清空：袋子与"当前播放"都属于旧角色
    pub fn clear(&mut self) {
        self.current = None;
        self.bags.clear();
        self.last_scene.clear();
    }

    /// 为指定状态重新开一个场景（状态切换 / 启动 / 切角色后调用），返回要播的第一步
    pub fn reset(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        self.current = None;
        let scenes = valid_scenes(persona, state);
        let scene = self.pick_scene(state, &scenes)?;
        self.start_step(persona, state, scene, 0, now)
    }

    /// 到点就推进：先走同场景的下一步，走完再按权重抽下一个场景
    pub fn advance(
        &mut self,
        persona: &PersonaConfig,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let current = self.current.clone()?;
        if now < current.next_at {
            return None;
        }
        let scenes = valid_scenes(persona, &current.state);
        if let Some(scene) = scenes.iter().find(|scene| scene.id == current.scene_id) {
            // 同场景还有下一步 → 继续走（沿用场景起始时刻，别把"这段已播多久"重置掉）
            if scene.steps.len() > current.step_index + 1 {
                return self.start_step_with_start(
                    persona,
                    &current.state,
                    scene,
                    current.step_index + 1,
                    now,
                    current.scene_started_at,
                );
            }
            // 已走完一遍：如果整段还太短，就从头再走一遍同一个场景。
            // 判据用「已播时长 + 本遍时长的一半」，让最终时长贴近 MIN_SCENE_HOLD_MS 而不是明显超出。
            let played_ms = (now - current.scene_started_at).num_milliseconds();
            let pass_ms = now
                .signed_duration_since(current.next_at)
                .num_milliseconds()
                .max(0);
            if played_ms + pass_ms / 2 < MIN_SCENE_HOLD_MS {
                return self.start_step_with_start(
                    persona,
                    &current.state,
                    scene,
                    0,
                    now,
                    current.scene_started_at,
                );
            }
        }
        let scene = self.pick_scene(&current.state, &scenes)?;
        self.start_step(persona, &current.state, scene, 0, now)
    }

    /// 下一次需要推进的时刻（并入引擎节拍线程的唤醒计算）
    pub fn next_at(&self) -> Option<DateTime<Local>> {
        self.current.as_ref().map(|current| current.next_at)
    }

    /// 是否到了推进时刻。引擎先用它判断，没到点就直接返回，
    /// 避免每次节拍唤醒都去克隆整份 persona 配置。
    pub fn is_due(&self, now: DateTime<Local>) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| now >= current.next_at)
    }

    /// 当前在播的动作 id
    pub fn current_clip(&self) -> Option<&str> {
        self.current.as_ref().map(|current| current.clip.as_str())
    }

    /// 当前正在播放的完整快照（对话 system prompt 需要动作说明，而不仅是 id）
    pub(crate) fn current(&self) -> Option<&Current> {
        self.current.as_ref()
    }

    /// 当前动作还剩多少毫秒（气泡避开"动作马上要切"的时刻）
    pub fn remaining_ms(&self, now: DateTime<Local>) -> Option<i64> {
        self.current
            .as_ref()
            .map(|current| (current.next_at - now).num_milliseconds())
    }

    /// 装配场景池：只保留至少有一个有效动作的场景（手改配置时的兜底）
    fn pick_scene<'a>(
        &mut self,
        state: &str,
        scenes: &[&'a SceneConfig],
    ) -> Option<&'a SceneConfig> {
        if scenes.is_empty() {
            return None;
        }
        // 袋空：按权重装填（权重 k 就放 k 份），洗牌后逐条弹出
        if self.bags.get(state).is_none_or(|bag| bag.is_empty()) {
            let mut pool: Vec<String> = Vec::new();
            for scene in scenes {
                for _ in 0..scene.weight.max(1) {
                    pool.push(scene.id.clone());
                }
            }
            let last = self.last_scene.get(state).cloned();
            self.bags
                .insert(state.to_string(), refill_bag(pool, last.as_deref()));
        }
        // 取一个「和上一次不同」的场景：同一动作连续播两次会看起来像被自己打断
        let last = self.last_scene.get(state).cloned();
        let bag = self.bags.get_mut(state)?;
        let index = bag
            .iter()
            .position(|id| Some(id) != last.as_ref())
            .unwrap_or(0);
        let id = bag.remove(index)?;
        // 袋里残留的失效场景 id（手改配置/切角色）直接丢弃后重挑
        let picked = match scenes.iter().find(|scene| scene.id == id) {
            Some(scene) => *scene,
            None => return self.pick_scene(state, scenes),
        };
        self.last_scene.insert(state.to_string(), id);
        Some(picked)
    }

    fn start_step(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        scene: &SceneConfig,
        index: usize,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        self.start_step_with_start(persona, state, scene, index, now, now)
    }

    /// 开播某一步；`scene_started_at` 用来保留场景的起始时刻（同场景循环时不被重置）
    fn start_step_with_start(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        scene: &SceneConfig,
        index: usize,
        now: DateTime<Local>,
        scene_started_at: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let step = scene.steps.get(index)?;
        let clip: &AnimationClipConfig = persona.clips.get(&step.clip)?;
        let loops = step.loops.clamp(1, MAX_STEP_LOOPS);
        let frames = clip.frames.max(1);
        let frame_ms = clip.frame_ms.max(1);
        let duration_ms = frames as u64 * frame_ms * loops as u64;
        self.current = Some(Current {
            state: state.to_string(),
            scene_id: scene.id.clone(),
            scene_label: scene.label.clone(),
            scene_started_at,
            step_index: index,
            clip: step.clip.clone(),
            next_at: now + chrono::Duration::milliseconds(duration_ms as i64),
        });
        Some(PlaybackEvent {
            state: state.to_string(),
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

/// 某状态下可播放的场景：至少有一个步骤引用了已定义的动作
fn valid_scenes<'a>(persona: &'a PersonaConfig, state: &str) -> Vec<&'a SceneConfig> {
    persona
        .scenes
        .get(state)
        .map(|scenes| {
            scenes
                .iter()
                .filter(|scene| {
                    scene
                        .steps
                        .iter()
                        .any(|step| persona.clips.contains_key(&step.clip))
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

    /// 一个状态两个场景：a（单步 2 帧×100ms）、b（两步：先 3 帧、再 2 帧）
    fn persona() -> PersonaConfig {
        let mut clips = HashMap::new();
        clips.insert("one".to_string(), clip(2, 100));
        clips.insert("two".to_string(), clip(3, 100));
        clips.insert("three".to_string(), clip(2, 100));
        let mut scenes = HashMap::new();
        scenes.insert(
            "idle".to_string(),
            vec![
                SceneConfig {
                    id: "a".into(),
                    label: "甲".into(),
                    weight: 1,
                    steps: vec![SceneStepConfig {
                        clip: "one".into(),
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
                            loops: 1,
                        },
                        SceneStepConfig {
                            clip: "three".into(),
                            loops: 2,
                        },
                    ],
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
                "idle".to_string(),
                StateConfig {
                    label: "待机".into(),
                    talkativeness: String::new(),
                    tone: String::new(),
                },
            )]),
            clips,
            scenes,
            schedule: ScheduleConfig {
                r#loop: vec![],
                time: vec![],
            },
        }
    }

    #[test]
    fn reset_starts_first_step_and_reports_duration() {
        let p = persona();
        let mut playback = Playback::default();
        let now = at(10, 0, 0);
        let event = playback.reset(&p, "idle", now).unwrap();
        assert!(event.clip == "one" || event.clip == "two");
        assert_eq!(
            event.duration_ms,
            event.frames as u64 * event.frame_ms * event.loops as u64
        );
        assert_eq!(
            playback.next_at(),
            Some(now + chrono::Duration::milliseconds(event.duration_ms as i64))
        );
        assert_eq!(playback.current_clip(), Some(event.clip.as_str()));
    }

    #[test]
    fn multi_step_scene_advances_step_by_step() {
        let p = persona();
        let mut playback = Playback::default();
        // 固定从两步场景 b 开始，验证步骤推进
        let now = at(10, 0, 0);
        let scenes = valid_scenes(&p, "idle");
        let scene_b = *scenes.iter().find(|s| s.id == "b").unwrap();
        let first = playback.start_step(&p, "idle", scene_b, 0, now).unwrap();
        assert_eq!(first.clip, "two");
        assert_eq!(first.step_index, 0);
        // 未到点不推进
        assert!(playback
            .advance(&p, now + chrono::Duration::milliseconds(100))
            .is_none());
        // 到点推进到第二步，且 loops=2 让时长翻倍
        let second = playback
            .advance(&p, now + chrono::Duration::milliseconds(300))
            .unwrap();
        assert_eq!(second.clip, "three");
        assert_eq!(second.step_index, 1);
        assert_eq!(second.duration_ms, 2 * 100 * 2);
    }

    #[test]
    fn every_scene_appears_within_one_bag_round() {
        let p = persona();
        let mut playback = Playback::default();
        let mut seen: Vec<String> = Vec::new();
        // 洗牌袋保证一轮内每个场景都出现，不会一直只抽到同一个
        for minute in 0..20 {
            let event = playback.reset(&p, "idle", at(10, minute, 0)).unwrap();
            if !seen.contains(&event.scene_id) {
                seen.push(event.scene_id.clone());
            }
        }
        seen.sort();
        assert_eq!(seen, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn scene_pick_never_repeats_back_to_back() {
        let p = persona();
        let mut playback = Playback::default();
        let mut previous = String::new();
        // 连续重开 10 次：同一个动作连着播两次会看起来像"被自己打断"
        for minute in 0..10 {
            let event = playback.reset(&p, "idle", at(10, minute, 0)).unwrap();
            assert_ne!(event.scene_id, previous, "连续两次抽到同一个场景");
            previous = event.scene_id;
        }
    }

    #[test]
    fn scene_holds_for_min_duration_before_switching() {
        let p = persona();
        let mut playback = Playback::default();
        let start = at(10, 0, 0);
        let first = playback.reset(&p, "idle", start).unwrap();
        let mut now = start + chrono::Duration::milliseconds(first.duration_ms as i64);
        let mut switched_at = None;
        for _ in 0..1000 {
            match playback.advance(&p, now) {
                Some(event) => {
                    if event.scene_id != first.scene_id {
                        switched_at = Some(now);
                        break;
                    }
                    now += chrono::Duration::milliseconds(event.duration_ms as i64);
                }
                None => break,
            }
        }
        let switched_at = switched_at.expect("场景最终应切换");
        let held_ms = (switched_at - start).num_milliseconds();
        assert!(
            (MIN_SCENE_HOLD_MS - 500..=MIN_SCENE_HOLD_MS + 1500).contains(&held_ms),
            "场景保持时长应接近 {MIN_SCENE_HOLD_MS}ms，实际 {held_ms}ms"
        );
    }

    #[test]
    fn clear_drops_current_and_bags() {
        let p = persona();
        let mut playback = Playback::default();
        playback.reset(&p, "idle", at(10, 0, 0));
        playback.clear();
        assert!(playback.current_clip().is_none());
        assert!(playback.next_at().is_none());
    }
}
