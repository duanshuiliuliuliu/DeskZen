//! 播放调度：由 Rust 决定「状态内走哪条链、当前播哪一段的哪个动作实例」，前端只按事件播放。
//!
//! 层级：状态（固定 6 个）→ 链（链内有序、链间可选）→ 段（有起止的 UI 表现）→ 动作实例（clip）。
//! 台词挂在动作上、且只在动作实例内出现，保证"说的"和"演的"严格一致。

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Local};
use serde::Serialize;

use crate::engine::{
    refill_bag, rng_unit, AnimationClipConfig, ChainConfig, PersonaConfig, SegmentConfig,
    SceneStepConfig,
};

/// 单个动作实例的时长上限（毫秒）：防止手改配置把角色卡在同一个动作上
pub const MAX_ACTION_MS: i64 = 5 * 60 * 1000;
/// 播放速度抖动范围：±8%，让同一个循环不至于每次都是同一节奏
const SPEED_MIN: f64 = 0.92;
const SPEED_MAX: f64 = 1.08;
/// 一段至少持续多久（毫秒）：作者只配了很短的一两拍时，重复这一段，
/// 避免几秒钟就换一件事做（重复时会重新掷概率/抖动，不是原样循环）
const MIN_SEGMENT_MS: i64 = 12_000;
/// 当日计划：今天想多做的标签，权重 ×1.6
const PLAN_FOCUS_FACTOR: f64 = 1.6;
/// 当日计划：今天想少做的标签，权重 ×0.45（降到"偶尔来一下"，不是禁掉）
const PLAN_AVOID_FACTOR: f64 = 0.45;
/// "注意你"实例在事件里的链/段标识与标题
const ACK_CHAIN_ID: &str = "__ack__";
const ACK_SEGMENT_ID: &str = "__ack__";
const ACK_LABEL: &str = "注意到你";

/// 一拍：段内的一个动作实例（已按概率筛过、时长/速度/相位已随机定好）
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedBeat {
    pub(crate) clip: String,
    loops: u32,
    duration_ms: u64,
    /// 播放速度倍率（0.92~1.08）：前端按它调整动画时长
    speed: f64,
    /// 起始帧相位：从循环的第几帧开始播（避免每次都从第 1 帧起）
    phase_frames: u32,
}

/// 一次段播放计划：段（或「注意你」反应）在本遍展开成哪些拍、共多久
#[derive(Debug, Clone)]
pub(crate) struct SegmentRun {
    pub(crate) chain_id: String,
    pub(crate) chain_label: String,
    pub(crate) scene_id: String,
    pub(crate) scene_label: String,
    /// 本段开始时刻（说话窗口按"段"而不是按"拍"计算）
    pub(crate) started_at: DateTime<Local>,
    /// 本段总时长 = 各拍之和
    pub(crate) total_ms: u64,
    pub(crate) beats: Vec<ResolvedBeat>,
    /// 是否是「注意你」的短反应
    pub(crate) ack: bool,
}

/// 发给前端的播放指令（前端只负责按它渲染，不再自己挑场景）
#[derive(Debug, Clone, PartialEq, Serialize)]
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
    /// 本拍的播放速度倍率（前端用它算动画时长）
    pub speed: f64,
    /// 本拍的起始帧相位（前端用它做负 animation-delay）
    pub phase_frames: u32,
}

/// 当前正在播放的动作实例
#[derive(Debug, Clone)]
pub(crate) struct Current {
    pub(crate) state: String,
    /// 本段（或反应）的播放计划
    pub(crate) run: SegmentRun,
    /// 当前是第几拍（0 起）
    pub(crate) beat_index: usize,
    pub(crate) clip: String,
    pub(crate) next_at: DateTime<Local>,
}

/// 播放调度状态（与状态引擎同生命周期，不持久化）
#[derive(Debug, Default)]
pub struct Playback {
    current: Option<Current>,
    /// 被打断的动作：播完"注意你"后回到它继续（只保留一层，够用且不会无限套娃）
    suspended: Option<Current>,
    /// 编排记忆：最近播过哪些动作、每条链上次/今天播了几次（`when` 约束靠它判断）。
    /// 跨状态保留（"战斗之后"正是跨状态因果），只在切换角色时清空。
    history: History,
    /// 需求快照（引擎每次推进前写入）：用来按标签调制链权重
    needs: crate::needs::NeedsState,
    /// 当日计划的活动偏好：今天想多做 / 少做的标签（空 = 不调制）
    plan_focus: Vec<String>,
    plan_avoid: Vec<String>,
    /// state -> 链 id 的加权洗牌袋：一轮内每条链按权重出现；链内顺序不受影响
    bags: HashMap<String, VecDeque<String>>,
    /// state -> 上一轮最后播放的链（跨轮防连续重复）
    last_chain: HashMap<String, String>,
}

/// 当天日期键（用于 max_per_day 计数）
fn today_key(now: DateTime<Local>) -> String {
    now.format("%Y-%m-%d").to_string()
}

/// 编排记忆
#[derive(Debug, Default)]
struct History {
    /// clip id -> 最近一次播放时刻
    recent_clips: HashMap<String, DateTime<Local>>,
    /// chain id -> 最近一次播放时刻
    chain_last: HashMap<String, DateTime<Local>>,
    /// chain id -> (日期, 当天播放次数)
    chain_today: HashMap<String, (String, u32)>,
}

impl Playback {
    /// 切换角色时清空：袋子与"当前播放"都属于旧角色
    pub fn clear(&mut self) {
        self.current = None;
        self.suspended = None;
        self.history = History::default();
        self.bags.clear();
        self.last_chain.clear();
    }

    /// 引擎推进前写入当前需求快照（`chain_bias` 用它调制权重）
    pub fn set_needs(&mut self, needs: crate::needs::NeedsState) {
        self.needs = needs;
    }

    /// 引擎推进前写入当日计划的活动偏好（focus 加分、avoid 减分，都按链的 tags 匹配）
    pub fn set_plan_tags(&mut self, focus: Vec<String>, avoid: Vec<String>) {
        self.plan_focus = focus;
        self.plan_avoid = avoid;
    }

    /// 当日计划对这条链的权重倍率：命中 focus ×1.6、命中 avoid ×0.45（同时命中则相乘）
    fn plan_factor(&self, tags: &[String]) -> f64 {
        let hit = |list: &[String]| tags.iter().any(|tag| list.contains(tag));
        let mut factor = 1.0;
        if hit(&self.plan_focus) {
            factor *= PLAN_FOCUS_FACTOR;
        }
        if hit(&self.plan_avoid) {
            factor *= PLAN_AVOID_FACTOR;
        }
        factor
    }

    /// 这条链当前的触发约束是否满足（`when` 没配 = 随时可用）。
    /// - `requires_recent`：最近 `within_min` 分钟内播过其中任一动作；
    /// - `cooldown_min`：距上次播这条链已超过冷却；
    /// - `max_per_day`：当天出现次数未达上限。
    fn chain_allowed(&self, chain: &ChainConfig, now: DateTime<Local>) -> bool {
        let Some(when) = &chain.when else {
            return true;
        };
        if !when.requires_recent.is_empty() {
            let within = when.within_minutes();
            let hit = when.requires_recent.iter().any(|clip| {
                self.history
                    .recent_clips
                    .get(clip)
                    .is_some_and(|at| (now - *at).num_minutes() < within)
            });
            if !hit {
                return false;
            }
        }
        if let Some(cooldown) = when.cooldown_min {
            if self
                .history
                .chain_last
                .get(&chain.id)
                .is_some_and(|at| (now - *at).num_minutes() < cooldown as i64)
            {
                return false;
            }
        }
        if let Some(max) = when.max_per_day {
            let today = today_key(now);
            if self
                .history
                .chain_today
                .get(&chain.id)
                .is_some_and(|(date, count)| *date == today && *count >= max)
            {
                return false;
            }
        }
        true
    }

    /// 记住"这条链开播了"（冷却与每日上限都基于它）
    fn remember_chain(&mut self, chain_id: &str, now: DateTime<Local>) {
        self.history
            .chain_last
            .insert(chain_id.to_string(), now);
        let today = today_key(now);
        let entry = self
            .history
            .chain_today
            .entry(chain_id.to_string())
            .or_insert_with(|| (today.clone(), 0));
        if entry.0 != today {
            *entry = (today, 1);
        } else {
            entry.1 += 1;
        }
    }

    /// 是否正处于"被打断去注意用户"的状态
    pub fn is_acknowledging(&self) -> bool {
        self.current.as_ref().is_some_and(|current| current.run.ack)
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
        let chain = self.pick_chain(persona, state, now)?;
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
        let beats = resolve_beats(persona, steps);
        if beats.is_empty() {
            self.suspended = None;
            return None;
        }
        let run = SegmentRun {
            chain_id: ACK_CHAIN_ID.into(),
            chain_label: ACK_LABEL.into(),
            scene_id: ACK_SEGMENT_ID.into(),
            scene_label: ACK_LABEL.into(),
            started_at: now,
            total_ms: beats.iter().map(|beat| beat.duration_ms).sum(),
            beats,
            ack: true,
        };
        self.start_beat(persona, state, &run, 0, now)
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
        // 1) 同一段还有下一拍（本段的节拍在开段时就已定好，含概率筛选与时长/速度抖动）
        if let Some(event) = self.start_beat(
            persona,
            &current.state,
            &current.run,
            current.beat_index + 1,
            now,
        ) {
            return Some(event);
        }
        // 2) 「注意你」反应播完 → 回到被打断的活动
        if current.run.ack {
            return self.resume_suspended(persona, now);
        }
        let chain = find_chain(persona, &current.state, &current.run.chain_id);
        let segment = chain.and_then(|chain| find_segment(chain, &current.run.scene_id));
        // 2.5) 这一段太短 → 再走一遍（重新掷概率/抖动），到够久才换段
        if let (Some(chain), Some(segment)) = (chain, segment) {
            let played_ms = (now - current.run.started_at).num_milliseconds();
            let pass_ms = current.run.total_ms as i64;
            if played_ms + pass_ms / 2 < MIN_SEGMENT_MS {
                let mut run = plan_segment(persona, chain, segment, now);
                // 保留本段的起始时刻：否则每次重播都把"已播多久"清零，永远攒不满最短时长
                run.started_at = current.run.started_at;
                if let Some(event) = self.start_beat(persona, &current.state, &run, 0, now) {
                    return Some(event);
                }
            }
        }
        // 3) 同一条链还有下一段（跳过没有有效动作的段）
        if let Some(chain) = chain {
            let pos = chain
                .segments
                .iter()
                .position(|segment| segment.id == current.run.scene_id);
            if let Some(pos) = pos {
                for segment in chain.segments.iter().skip(pos + 1) {
                    let run = plan_segment(persona, chain, segment, now);
                    if let Some(event) = self.start_beat(persona, &current.state, &run, 0, now) {
                        return Some(event);
                    }
                }
            }
        }
        // 4) 链走完 → 在满足约束的链里按权重换一条
        let chain = self.pick_chain(persona, &current.state, now)?;
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

    /// 从状态内的链里抽一条：先按 `when` 约束过滤，再按"权重 × 需求偏置"装洗牌袋。
    /// 一袋（一轮）里每条链恰好出现「份数」次，所以长期频率就是权重比（份数是整数，
    /// 需求偏置按倍率改份数）；抽的时候跳过"刚播过的那条"，避免同一件事连着做两遍
    /// ——袋里还有别的就先换一个，实在只剩它才破例。
    /// 链内段的顺序由作者定，绝不打乱——因果链靠这个保证。
    fn pick_chain<'a>(
        &mut self,
        persona: &'a PersonaConfig,
        state: &str,
        now: DateTime<Local>,
    ) -> Option<&'a ChainConfig> {
        let chains = valid_chains(persona, state);
        if chains.is_empty() {
            return None;
        }
        // 约束优先：先只在满足 when 的链里抽；全被挡下时退回不筛选，
        // 宁可偶尔破例也不让角色卡死（约束是"过滤"，不是"死锁"）
        let eligible: Vec<&ChainConfig> = chains
            .iter()
            .copied()
            .filter(|chain| self.chain_allowed(chain, now))
            .collect();
        // 被 when 约束挡下的链单独记一笔：排查"某条链一直不出现"时一眼能看出来
        if eligible.len() != chains.len() {
            let blocked: Vec<&str> = chains
                .iter()
                .filter(|chain| !eligible.iter().any(|c| c.id == chain.id))
                .map(|chain| chain.id.as_str())
                .collect();
            log::debug!("抽链 {state}：{} 条链被 when 约束挡下（{}）", blocked.len(), blocked.join("、"));
        }
        let chains = if eligible.is_empty() { chains } else { eligible };
        if self.bags.get(state).is_none_or(|bag| bag.is_empty()) {
            let mut pool: Vec<String> = Vec::new();
            for chain in &chains {
                // 需求偏置 × 当日计划偏置：命中标签的链按倍率放大份数，上下限防止爆量/清零
                let factor = persona
                    .needs
                    .chain_factor(&self.needs, &chain.tags)
                    .max(0.0)
                    * self.plan_factor(&chain.tags);
                let count = (chain.weight.max(1) as f64 * factor)
                    .round()
                    .clamp(1.0, 100.0) as u32;
                if (factor - 1.0).abs() > f64::EPSILON {
                    log::debug!(
                        "抽链 {state}：{} 权重 {} × 偏置 {factor:.2} → {count} 份",
                        chain.id,
                        chain.weight
                    );
                }
                for _ in 0..count {
                    pool.push(chain.id.clone());
                }
            }
            let last = self.last_chain.get(state).cloned();
            self.bags
                .insert(state.to_string(), refill_bag(pool, last.as_deref()));
        }
        let last = self.last_chain.get(state).cloned();
        let bag = self.bags.get_mut(state)?;
        // 跳过刚播过的那条（袋里还有别的就先换一个；只剩它时才不得不重复）
        let index = bag
            .iter()
            .position(|id| Some(id) != last.as_ref())
            .unwrap_or(0);
        let id = bag.remove(index)?;
        let remaining = bag.len();
        // 袋里残留的失效链 id 直接丢弃后重挑
        let picked = match chains.iter().find(|chain| chain.id == id) {
            Some(chain) => *chain,
            None => return self.pick_chain(persona, state, now),
        };
        log::debug!("抽链 {state}：选中 {}（袋里还剩 {remaining} 条）", picked.id);
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
        for segment in &chain.segments {
            let run = plan_segment(persona, chain, segment, now);
            if let Some(event) = self.start_beat(persona, state, &run, 0, now) {
                // 链真正开播了才记账（冷却/每日上限都基于它）
                self.remember_chain(&chain.id, now);
                return Some(event);
            }
        }
        None
    }

    /// 反应播完：回到被打断的那一拍重新开始；配置已不可用时按常规换链
    fn resume_suspended(
        &mut self,
        persona: &PersonaConfig,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let Some(suspended) = self.suspended.take() else {
            let state = self.current.as_ref()?.state.clone();
            let chain = self.pick_chain(persona, &state, now)?;
            return self.start_chain(persona, &state, chain, now);
        };
        // 回到被打断的那一拍：沿用它当时定好的时长/速度/相位，读起来才是"接着做"
        if let Some(event) = self.start_beat(
            persona,
            &suspended.state,
            &suspended.run,
            suspended.beat_index,
            now,
        ) {
            return Some(event);
        }
        let chain = self.pick_chain(persona, &suspended.state, now)?;
        self.start_chain(persona, &suspended.state, chain, now)
    }

    /// 开播某一段里的第 `index` 拍
    fn start_beat(
        &mut self,
        persona: &PersonaConfig,
        state: &str,
        run: &SegmentRun,
        index: usize,
        now: DateTime<Local>,
    ) -> Option<PlaybackEvent> {
        let beat = run.beats.get(index)?;
        let clip: &AnimationClipConfig = persona.clips.get(&beat.clip)?;
        // 记住这个动作最近播过（`when.requires_recent` 靠它判断因果）
        self.history
            .recent_clips
            .insert(beat.clip.clone(), now);
        let frames = clip.frames.max(1);
        let frame_ms = clip.frame_ms.max(1);
        let duration_ms = beat.duration_ms;
        log::debug!(
            "播放 {state} {} 第 {} 拍：{} ×{}（{:.1}s，速度 {:.2}）",
            run.scene_id,
            index,
            beat.clip,
            beat.loops,
            duration_ms as f64 / 1000.0,
            beat.speed
        );
        self.current = Some(Current {
            state: state.to_string(),
            run: run.clone(),
            beat_index: index,
            clip: beat.clip.clone(),
            next_at: now + chrono::Duration::milliseconds(duration_ms as i64),
        });
        Some(PlaybackEvent {
            state: state.to_string(),
            chain_id: run.chain_id.clone(),
            chain_label: run.chain_label.clone(),
            scene_id: run.scene_id.clone(),
            scene_label: run.scene_label.clone(),
            step_index: index as u32,
            clip: beat.clip.clone(),
            spritesheet: clip.spritesheet.clone(),
            frames,
            frame_ms,
            loops: beat.loops,
            duration_ms,
            speed: beat.speed,
            phase_frames: beat.phase_frames,
        })
    }
}

/// 把一段展开成本遍的节拍：按 `chance` 筛掉这次不出现的拍，并给每拍定好
/// 时长（`seconds` 数字或区间）、速度抖动与起始相位。全部被筛掉时退回完整列表，
/// 保证段不会空转。
fn plan_segment(
    persona: &PersonaConfig,
    chain: &ChainConfig,
    segment: &SegmentConfig,
    now: DateTime<Local>,
) -> SegmentRun {
    let beats = resolve_beats(persona, &segment.steps);
    let total_ms = beats.iter().map(|beat| beat.duration_ms).sum();
    SegmentRun {
        chain_id: chain.id.clone(),
        chain_label: chain.label.clone(),
        scene_id: segment.id.clone(),
        scene_label: segment.label.clone(),
        started_at: now,
        total_ms,
        beats,
        ack: false,
    }
}

/// 「注意你」反应也走同一套节拍解析（一般只有一拍）
fn resolve_beats(persona: &PersonaConfig, steps: &[SceneStepConfig]) -> Vec<ResolvedBeat> {
    let beats: Vec<ResolvedBeat> = steps
        .iter()
        .filter(|step| persona.clips.contains_key(&step.clip))
        .filter(|step| step.chance >= 1.0 || rng_unit() < step.chance)
        .filter_map(|step| resolve_beat(persona, step))
        .collect();
    if beats.is_empty() {
        steps
            .iter()
            .filter(|step| persona.clips.contains_key(&step.clip))
            .filter_map(|step| resolve_beat(persona, step))
            .collect()
    } else {
        beats
    }
}

/// 单拍解析：目标时长 → 整数个循环；速度 ±8%；相位随机（从循环中间某帧起播）
fn resolve_beat(persona: &PersonaConfig, step: &SceneStepConfig) -> Option<ResolvedBeat> {
    let clip = persona.clips.get(&step.clip)?;
    let frames = clip.frames.max(1);
    let frame_ms = clip.frame_ms.max(1);
    let speed = SPEED_MIN + rng_unit() * (SPEED_MAX - SPEED_MIN);
    // 速度抖动会改变单圈实际时长，时长与循环数都按"抖动后的一圈"算，
    // 前端按 speed 调整动画时长，两边才对得上。
    let native_ms = (frames as f64 * frame_ms as f64 / speed).round().max(1.0) as i64;
    let max_loops = ((MAX_ACTION_MS / native_ms).max(1)) as u32;
    let loops = match &step.seconds {
        Some(spec) => {
            (spec.sample_ms() as f64 / native_ms as f64).round().clamp(1.0, max_loops as f64) as u32
        }
        // 不写时长 = 只播一遍
        None => 1,
    };
    let duration_ms = (loops as i64 * native_ms).max(1) as u64;
    Some(ResolvedBeat {
        clip: step.clip.clone(),
        loops,
        duration_ms,
        speed,
        phase_frames: (rng_unit() * frames as f64) as u32 % frames,
    })
}

/// 链内按 id 找段（段现在内联在链里，不再有独立段表）
fn find_segment<'a>(chain: &'a ChainConfig, segment_id: &str) -> Option<&'a SegmentConfig> {
    chain.segments.iter().find(|segment| segment.id == segment_id)
}

fn find_chain<'a>(
    persona: &'a PersonaConfig,
    state: &str,
    chain_id: &str,
) -> Option<&'a ChainConfig> {
    persona
        .states
        .get(state)?
        .chains
        .iter()
        .find(|chain| chain.id == chain_id)
}

/// 某状态下可用的链：至少有一个段包含已定义动作
fn valid_chains<'a>(persona: &'a PersonaConfig, state: &str) -> Vec<&'a ChainConfig> {
    persona
        .states
        .get(state)
        .map(|cfg| {
            cfg.chains
                .iter()
                .filter(|chain| {
                    chain.segments.iter().any(|segment| {
                        segment
                            .steps
                            .iter()
                            .any(|step| persona.clips.contains_key(&step.clip))
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

    /// 造一段：`id`/`label` 都给全，steps 是段内的拍
    fn seg(id: &str, label: &str, steps: Vec<SceneStepConfig>) -> SegmentConfig {
        SegmentConfig {
            id: id.into(),
            label: label.into(),
            steps,
        }
    }

    /// 一拍：不写 seconds = 只播一遍
    fn step(clip: &str) -> SceneStepConfig {
        SceneStepConfig {
            clip: clip.into(),
            ..Default::default()
        }
    }

    /// 一拍：指定目标秒数
    fn step_secs(clip: &str, seconds: u32) -> SceneStepConfig {
        SceneStepConfig {
            clip: clip.into(),
            seconds: Some(crate::engine::SecondsSpec::Fixed(seconds)),
            ..Default::default()
        }
    }

    /// 整体替换某状态的活动链
    fn set_chains(p: &mut PersonaConfig, state: &str, chains: Vec<ChainConfig>) {
        p.states.get_mut(state).unwrap().chains = chains;
    }

    /// 取某状态第 `chain` 条链的第 `segment` 个段
    fn segment_mut<'a>(
        p: &'a mut PersonaConfig,
        state: &str,
        chain: usize,
        segment: usize,
    ) -> &'a mut SegmentConfig {
        &mut p.states.get_mut(state).unwrap().chains[chain].segments[segment]
    }

    /// 用现成的段造一条链
    fn chain_of(id: &str, weight: u32, segments: Vec<SegmentConfig>, tags: Vec<&str>) -> ChainConfig {
        ChainConfig {
            id: id.into(),
            label: id.into(),
            weight,
            segments,
            steps: vec![],
            tags: tags.into_iter().map(str::to_string).collect(),
            when: None,
        }
    }

    /// 取某状态第一条链的段（clone，供造新链用）
    fn segments_of(p: &PersonaConfig, state: &str) -> Vec<SegmentConfig> {
        p.states[state].chains[0].segments.clone()
    }

    /// 一个状态两条链：
    /// - chain_ab（甲 → 乙）：甲单步（200ms 原生，目标 1 秒）；乙两步（播一遍 + 目标 1 秒）
    /// - chain_b：只有乙
    fn persona() -> PersonaConfig {
        let mut clips = HashMap::new();
        clips.insert("one".to_string(), clip(2, 100)); // 原生 200ms
        clips.insert("two".to_string(), clip(3, 100)); // 原生 300ms
        clips.insert("three".to_string(), clip(2, 100)); // 原生 200ms
        let a = seg("a", "甲", vec![step_secs("one", 1)]);
        let b = seg("b", "乙", vec![step("two"), step_secs("three", 1)]);
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
                    chains: vec![
                        ChainConfig {
                            id: "chain_ab".into(),
                            label: "甲→乙".into(),
                            weight: 1,
                            segments: vec![a.clone(), b.clone()],
                            steps: vec![],
                            tags: vec![],
                            when: None,
                        },
                        ChainConfig {
                            id: "chain_b".into(),
                            label: "乙".into(),
                            weight: 1,
                            segments: vec![b.clone()],
                            steps: vec![],
                            tags: vec![],
                            when: None,
                        },
                    ],
                },
            )]),
            clips,
            clips_dir: String::new(),
            // 默认给所有状态配一个 1 秒的"注意你"反应，供 acknowledge 用例使用
            acknowledge: crate::engine::AcknowledgeConfig {
                default: vec![step_secs("three", 1)],
                ..Default::default()
            },
            needs: crate::needs::NeedsConfig::default(),
            schedule: ScheduleConfig {
                r#loop: vec![],
                time: vec![],
            },
        }
    }

    /// 单链版本，用于确定性地验证链内顺序
    fn single_chain_persona() -> PersonaConfig {
        let mut p = persona();
        p.states.get_mut("routine").unwrap().chains.truncate(1);
        p
    }

    /// 长动作实例版本：「注意你」的打断/恢复用例需要当前动作还剩足够时间
    fn long_instance_persona() -> PersonaConfig {
        let mut p = single_chain_persona();
        for chain in &mut p.states.get_mut("routine").unwrap().chains {
            for segment in &mut chain.segments {
                for beat in &mut segment.steps {
                    beat.seconds = Some(crate::engine::SecondsSpec::Fixed(30));
                }
            }
        }
        p
    }

    #[test]
    fn seconds_target_rounds_to_whole_loops_within_jitter() {
        let p = single_chain_persona();
        let mut playback = Playback::default();
        let now = at(10, 0, 0);
        let event = playback.reset(&p, "routine", now).unwrap();
        assert_eq!(event.scene_id, "a");
        assert_eq!(event.clip, "one");
        // 目标 1 秒、原生 200ms → 5 个循环（速度抖动会改单圈时长，但循环数不变）
        assert_eq!(event.loops, 5);
        // 速度抖动 ±8%：时长落在目标附近，但不再是精确的 1000ms
        assert!((SPEED_MIN..=SPEED_MAX).contains(&event.speed));
        assert!(
            (event.duration_ms as i64 - 1000).abs() <= 100,
            "时长应接近目标 1000ms，实际 {}ms",
            event.duration_ms
        );
        assert!(event.phase_frames < event.frames.max(1), "相位必须在帧数范围内");
        assert_eq!(
            playback.next_at(),
            Some(now + chrono::Duration::milliseconds(event.duration_ms as i64))
        );
    }

    #[test]
    fn advance_follows_chain_and_segment_steps() {
        let p = single_chain_persona();
        let mut playback = Playback::default();
        let start = at(10, 0, 0);
        let first = playback.reset(&p, "routine", start).unwrap();
        assert_eq!(first.scene_id, "a");
        assert_eq!(first.chain_id, "chain_ab");

        // 段 a 只有 1 秒（<12 秒最短保持）→ 会重复本段，直到累计够长才换到 b
        let mut now = start + chrono::Duration::milliseconds(first.duration_ms as i64);
        let mut second = None;
        for _ in 0..300 {
            let event = playback.advance(&p, now).unwrap();
            now += chrono::Duration::milliseconds(event.duration_ms as i64);
            if event.scene_id != "a" {
                second = Some(event);
                break;
            }
        }
        let second = second.expect("段 a 重复够久后应切到下一段");
        assert_eq!(second.scene_id, "b");
        assert_eq!(second.step_index, 0);
        assert_eq!(second.clip, "two");
        // 这一拍没写 seconds = 只播一遍："two" 原生 300ms
        assert_eq!(second.loops, 1, "不写 seconds 就是播一遍");
        assert!(
            (second.duration_ms as i64 - 300).abs() <= 60,
            "播一遍应接近 300ms，实际 {}ms",
            second.duration_ms
        );

        // b 的第一步播完 → 同段第二步（目标 1s）；段没走完不会被"最短保持"拦下
        let third = playback.advance(&p, now).unwrap();
        assert_eq!(third.scene_id, "b");
        assert_eq!(third.step_index, 1);
        assert_eq!(third.clip, "three");
        assert!((third.duration_ms as i64 - 1000).abs() <= 100);
    }

    #[test]
    fn beat_without_seconds_plays_single_loop() {
        let mut p = persona();
        // 只留"乙"这条链：首拍不写 seconds → 播一遍（"two" 原生 300ms）
        let b = p.states["routine"].chains[1].clone();
        set_chains(&mut p, "routine", vec![b]);
        let event = Playback::default().reset(&p, "routine", at(10, 0, 0)).unwrap();
        assert_eq!(event.clip, "two");
        assert_eq!(event.loops, 1);
        assert!(
            (event.duration_ms as i64 - 300).abs() <= 60,
            "播一遍应接近 300ms，实际 {}ms",
            event.duration_ms
        );
    }

    #[test]
    fn chain_selection_never_repeats_back_to_back() {
        let p = persona();
        let mut playback = Playback::default();
        let mut previous = String::new();
        let mut repeats = 0;
        let draws = 40;
        for minute in 0..draws {
            let event = playback.reset(&p, "routine", at(10, minute, 0)).unwrap();
            if event.chain_id == previous {
                repeats += 1;
            }
            previous = event.chain_id;
        }
        // 洗牌袋按权重给份数、抽的时候跳过刚播过的那条：份数够多时一次都不该连着重复
        // （只有"袋里只剩它"才不得不重复，见下面的权重倾斜用例）
        assert_eq!(repeats, 0, "连续两次抽到同一条链");
    }

    #[test]
    fn chain_weights_keep_their_share_within_a_bag() {
        // 权重 3:1 → 一袋（一轮）里份数就是 3 和 1，长期频率自然也是 3:1；
        // 需求偏置改的就是这个份数，所以"约束优先级"之后仍有稳定的份额保证
        let mut p = persona();
        let segs = segments_of(&p, "routine");
        set_chains(
            &mut p,
            "routine",
            vec![
                chain_of("heavy", 3, vec![segs[0].clone()], vec![]),
                chain_of("light", 1, vec![segs[1].clone()], vec![]),
            ],
        );
        let mut playback = Playback::default();
        let mut counts = std::collections::HashMap::new();
        for minute in 0..40 {
            let event = playback.reset(&p, "routine", at(10, minute, 0)).unwrap();
            *counts.entry(event.chain_id).or_insert(0) += 1;
        }
        assert_eq!(counts.get("heavy"), Some(&30), "重链应占 3/4");
        assert_eq!(counts.get("light"), Some(&10), "轻链应占 1/4");
    }

    #[test]
    fn seconds_range_stays_near_bounds_and_jitters() {
        let mut p = single_chain_persona();
        segment_mut(&mut p, "routine", 0, 0).steps[0].seconds =
            Some(crate::engine::SecondsSpec::Range([5, 8]));
        let mut durations = std::collections::HashSet::new();
        for _ in 0..20 {
            let mut playback = Playback::default();
            let event = playback.reset(&p, "routine", at(10, 0, 0)).unwrap();
            let ms = event.duration_ms as i64;
            // clip "one" 原生 200ms：区间 [5,8] 秒取整后落在 4.5~9 秒之间
            assert!((4_500..=9_000).contains(&ms), "区间取的时长越界: {ms}ms");
            durations.insert(ms);
        }
        assert!(durations.len() > 1, "区间+抖动应当产生不止一种时长");
    }

    #[test]
    fn chance_zero_skips_beat_and_missing_seconds_plays_once() {
        let mut p = single_chain_persona();
        // 让段 a 足够长（30 秒），免得被"最短保持"重复，方便直接走到段 b
        segment_mut(&mut p, "routine", 0, 0).steps[0].seconds =
            Some(crate::engine::SecondsSpec::Fixed(30));
        {
            let segment = segment_mut(&mut p, "routine", 0, 1); // 段 b：两步
            segment.steps[0].chance = 0.0; // 这一遍不出现
            segment.steps[1].seconds = None; // 不写时长 = 只播一遍
        }
        let mut playback = Playback::default();
        let now = at(10, 0, 0);
        let first = playback.reset(&p, "routine", now).unwrap();
        let second = playback
            .advance(&p, now + chrono::Duration::milliseconds(first.duration_ms as i64))
            .unwrap();
        assert_eq!(second.scene_id, "b");
        assert_eq!(second.clip, "three", "chance=0 的那一拍应被跳过");
        assert_eq!(second.loops, 1, "不写 seconds 就是播一遍");
    }

    /// 造一条带 when 约束的链（段 `a` 播 clip "one"，段 `b` 播 "two"+"three"）
    fn chain_with_when(
        id: &str,
        segments: Vec<SegmentConfig>,
        when: crate::engine::ChainWhen,
    ) -> ChainConfig {
        ChainConfig {
            id: id.into(),
            label: id.into(),
            weight: 1,
            segments,
            steps: vec![],
            tags: vec![],
            when: Some(when),
        }
    }

    #[test]
    fn requires_recent_gates_chain_and_expires() {
        use crate::engine::ChainWhen;
        let mut playback = Playback::default();
        let chain = chain_with_when(
            "aftermath",
            vec![seg("b", "乙", vec![step("two")])],
            ChainWhen {
                requires_recent: vec!["one".into()],
                within_min: Some(10),
                ..Default::default()
            },
        );
        let start = at(10, 0, 0);
        // 还没播过 "one" → 不可选
        assert!(!playback.chain_allowed(&chain, start));
        // 播过之后 5 分钟内可选
        playback
            .history
            .recent_clips
            .insert("one".into(), start);
        assert!(playback.chain_allowed(&chain, start + chrono::Duration::minutes(5)));
        // 超过有效期 → 再次不可选
        assert!(!playback.chain_allowed(&chain, start + chrono::Duration::minutes(11)));
        // 没配 when 的链随时可选
        let free = ChainConfig {
            when: None,
            ..chain.clone()
        };
        assert!(playback.chain_allowed(&free, start));
    }

    #[test]
    fn cooldown_and_daily_cap_limit_chain() {
        use crate::engine::ChainWhen;
        let mut playback = Playback::default();
        let start = at(10, 0, 0);
        let chain = chain_with_when(
            "maintenance",
            vec![seg("a", "甲", vec![step_secs("one", 1)])],
            ChainWhen {
                cooldown_min: Some(45),
                ..Default::default()
            },
        );
        assert!(playback.chain_allowed(&chain, start), "首次可选");
        playback.remember_chain(&chain.id, start);
        assert!(!playback.chain_allowed(&chain, start + chrono::Duration::minutes(30)));
        assert!(playback.chain_allowed(&chain, start + chrono::Duration::minutes(46)), "冷却结束");

        let capped = chain_with_when(
            "egg",
            vec![seg("a", "甲", vec![step_secs("one", 1)])],
            ChainWhen {
                max_per_day: Some(2),
                ..Default::default()
            },
        );
        let mut playback = Playback::default();
        assert!(playback.chain_allowed(&capped, start));
        playback.remember_chain(&capped.id, start);
        playback.remember_chain(&capped.id, start + chrono::Duration::minutes(5));
        assert!(!playback.chain_allowed(&capped, start + chrono::Duration::minutes(10)));
        // 跨天重置
        assert!(playback.chain_allowed(&capped, start + chrono::Duration::days(1)));
    }

    #[test]
    fn blocked_chains_never_starve_the_state() {
        use crate::engine::ChainWhen;
        let mut p = single_chain_persona();
        // 唯一的一条链被"最近播过某动作"挡住（那个动作永远不会出现）
        set_chains(
            &mut p,
            "routine",
            vec![chain_with_when(
                "impossible",
                vec![seg("a", "甲", vec![step_secs("one", 1)])],
                ChainWhen {
                    requires_recent: vec!["two".into()],
                    within_min: Some(10),
                    ..Default::default()
                },
            )],
        );
        let mut playback = Playback::default();
        let event = playback
            .reset(&p, "routine", at(10, 0, 0))
            .expect("约束全挡下时也要能播，不能卡死");
        assert_eq!(event.chain_id, "impossible");
    }

    #[test]
    fn plan_tags_bias_chain_weights() {
        // 当日计划：今天想多做 explore、少做 social（两条链权重相同，只看计划）
        let mut p = persona();
        let segs = segments_of(&p, "routine");
        set_chains(
            &mut p,
            "routine",
            vec![
                chain_of("explore_chain", 5, vec![segs[0].clone()], vec!["explore"]),
                chain_of("social_chain", 5, vec![segs[1].clone()], vec!["social"]),
            ],
        );
        let count_explore = |focus: Vec<String>, avoid: Vec<String>| {
            let mut playback = Playback::default();
            playback.set_plan_tags(focus, avoid);
            (0..40)
                .filter(|minute| {
                    playback
                        .reset(&p, "routine", at(10, *minute, 0))
                        .unwrap()
                        .chain_id
                        == "explore_chain"
                })
                .count()
        };
        // 没计划：权重相同 → 一袋 5:5，各占一半
        assert_eq!(count_explore(vec![], vec![]), 20);
        // 有计划：explore 份数 5×1.6=8、social 5×0.45≈2 → 一袋 8:2
        assert_eq!(
            count_explore(vec!["explore".into()], vec!["social".into()]),
            32
        );
    }

    #[test]
    fn needs_bias_shifts_chain_distribution() {
        use crate::needs::ChainBias;
        let mut p = persona();
        let segs = segments_of(&p, "routine");
        set_chains(
            &mut p,
            "routine",
            vec![
                chain_of("social_chain", 1, vec![segs[0].clone()], vec!["social"]),
                chain_of("plain_1", 1, vec![segs[1].clone()], vec![]),
                chain_of("plain_2", 1, vec![segs[1].clone()], vec![]),
            ],
        );
        p.needs.chain_bias = vec![ChainBias {
            need: "social".into(),
            above: Some(0.5),
            below: None,
            tag: "social".into(),
            factor: 20.0,
        }];

        let count_social = |social: f64| {
            let mut playback = Playback::default();
            playback.set_needs(crate::needs::NeedsState {
                social,
                ..Default::default()
            });
            (0..60)
                .filter(|minute| {
                    playback
                        .reset(&p, "routine", at(10, *minute, 0))
                        .unwrap()
                        .chain_id
                        == "social_chain"
                })
                .count()
        };
        let high = count_social(0.9);
        let low = count_social(0.1);
        // 社交欲高时 social 链的份数被放大 20 倍，袋里它占绝大多数；
        // 两条无标签链只在"刚播过 social"时被插进来，所以不是 100%，但差距必须明显
        assert!(
            high > low + 10 && high > 30,
            "社交欲高时应明显更多走 social 标签的链：high={high} low={low}"
        );
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
