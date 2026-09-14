//! 内部需求 / 情绪：让编排"有内部状态"，而不是只看时间表和权重。
//!
//! 四个 0~1 的标量：
//! - `energy`  体力：平时缓慢下降，睡觉/休息/吃饭回升；低于阈值会把角色"拉"去休息或睡觉
//! - `hunger`  饥饿：随时间上升，吃饭下降；高于阈值会把角色拉去吃饭
//! - `boredom` 无聊：待久了上升，换新链/被搭话下降；高时偏好"探索"类链
//! - `social`  社交欲：长时间没人理上升，被注意到下降；高时偏好"社交"类链
//!
//! 优先级铁律：**硬时段 > 需求 > 权重**——时段是作者写死的硬约束，需求只在时段之外起作用。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 需求的名字（配置与标签用同一套键）
pub(crate) const NEED_NAMES: &[&str] = &["energy", "hunger", "boredom", "social"];

/// 内置默认演化率（每小时）：正 = 涨，负 = 降
fn default_rate(need: &str) -> f64 {
    match need {
        "energy" => -0.055,
        "hunger" => 0.075,
        "boredom" => 0.06,
        "social" => 0.09,
        _ => 0.0,
    }
}

/// 内置默认：处于某个状态时的覆盖率（睡觉回体力、吃饭解饿…）
fn default_state_rate(need: &str, state: &str) -> Option<f64> {
    match (need, state) {
        ("energy", "sleep") => Some(0.35),
        ("energy", "relax") => Some(0.06),
        ("energy", "eat") => Some(0.08),
        ("hunger", "eat") => Some(-1.2),
        ("hunger", "sleep") => Some(-0.3),
        ("boredom", "sleep") => Some(-0.05),
        ("social", "sleep") => Some(-0.02),
        _ => None,
    }
}

fn clamp01(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 需求状态（可持久化到 needs.json，重启后按流逝时间补算）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeedsState {
    #[serde(default = "default_energy")]
    pub energy: f64,
    #[serde(default = "default_hunger")]
    pub hunger: f64,
    #[serde(default = "default_boredom")]
    pub boredom: f64,
    #[serde(default = "default_social")]
    pub social: f64,
    /// 上次推进的 Unix 时间戳（秒）；缺省 0 表示没有历史，不做补算
    #[serde(default)]
    pub updated_unix: i64,
}

fn default_energy() -> f64 {
    0.85
}
fn default_hunger() -> f64 {
    0.25
}
fn default_boredom() -> f64 {
    0.2
}
fn default_social() -> f64 {
    0.3
}

impl Default for NeedsState {
    fn default() -> Self {
        Self {
            energy: default_energy(),
            hunger: default_hunger(),
            boredom: default_boredom(),
            social: default_social(),
            updated_unix: 0,
        }
    }
}

impl NeedsState {
    pub fn get(&self, need: &str) -> f64 {
        match need {
            "energy" => self.energy,
            "hunger" => self.hunger,
            "boredom" => self.boredom,
            "social" => self.social,
            _ => 0.0,
        }
    }

    fn set(&mut self, need: &str, value: f64) {
        let value = clamp01(value);
        match need {
            "energy" => self.energy = value,
            "hunger" => self.hunger = value,
            "boredom" => self.boredom = value,
            "social" => self.social = value,
            _ => {}
        }
    }

    /// 某个需求在某个状态下的每小时变化率：persona 的 restore → 内置状态率 → persona 的 rates → 内置基准率
    fn rate(&self, need: &str, state: &str, cfg: &NeedsConfig) -> f64 {
        cfg.restore
            .get(need)
            .and_then(|by_state| by_state.get(state))
            .copied()
            .or_else(|| default_state_rate(need, state))
            .or_else(|| cfg.rates.get(need).copied())
            .unwrap_or_else(|| default_rate(need))
    }

    /// 按流逝时长推进（`state` 为这段时间里角色所处的状态，决定恢复/消耗）
    pub fn advance(&mut self, state: &str, hours: f64, cfg: &NeedsConfig) {
        if !hours.is_finite() || hours <= 0.0 {
            return;
        }
        for need in NEED_NAMES {
            let rate = self.rate(need, state, cfg);
            self.set(need, self.get(need) + rate * hours);
        }
    }

    /// 被用户注意到：社交欲与无聊一起下降
    pub fn on_seen(&mut self) {
        self.social = clamp01(self.social - 0.35);
        self.boredom = clamp01(self.boredom - 0.2);
    }

    /// 换了一条新链：新鲜感让无聊下降
    pub fn on_new_chain(&mut self) {
        self.boredom = clamp01(self.boredom - 0.15);
    }
}

/// persona 里的 `needs` 配置；不配就用内置默认
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NeedsConfig {
    /// 初始值覆盖（只在该角色第一次运行时生效），例如 {"energy": 0.6}
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub start: HashMap<String, f64>,
    /// need -> 每小时基准变化率
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub rates: HashMap<String, f64>,
    /// need -> 状态 -> 该状态下的每小时变化率（覆盖 rates）
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub restore: HashMap<String, HashMap<String, f64>>,
    /// 需求把角色"拉"到某个状态（按顺序取第一条命中的）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull: Vec<NeedPull>,
    /// 需求调制链权重：命中标签的链乘上 factor
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain_bias: Vec<ChainBias>,
}

impl NeedsConfig {
    /// 需求把角色拉向哪个状态：优先用 persona 配的 pull，没配则用内置默认
    pub(crate) fn pull_state(&self, needs: &NeedsState) -> Option<String> {
        if self.pull.is_empty() {
            return default_pull()
                .into_iter()
                .find(|rule| rule.matches(needs))
                .map(|rule| rule.state);
        }
        self.pull
            .iter()
            .find(|rule| rule.matches(needs))
            .map(|rule| rule.state.clone())
    }

    /// 这条链在当前需求下的权重倍率（1.0 = 不变）
    pub(crate) fn chain_factor(&self, needs: &NeedsState, tags: &[String]) -> f64 {
        let rules = if self.chain_bias.is_empty() {
            default_bias()
        } else {
            self.chain_bias.clone()
        };
        let mut factor = 1.0;
        for rule in &rules {
            if rule.matches(needs) && tags.iter().any(|tag| tag == &rule.tag) {
                factor *= rule.factor.max(0.0);
            }
        }
        factor
    }

    /// 有没有配置（用于决定是否需要校验）
    pub(crate) fn configured(&self) -> bool {
        !self.start.is_empty()
            || !self.rates.is_empty()
            || !self.restore.is_empty()
            || !self.pull.is_empty()
            || !self.chain_bias.is_empty()
    }

    /// 是否是"什么都没配"（serde 跳过序列化用）
    pub(crate) fn is_default(&self) -> bool {
        !self.configured()
    }
}

/// 一条"需求 → 状态"的拉取规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NeedPull {
    pub need: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    pub state: String,
}

impl NeedPull {
    pub(crate) fn matches(&self, needs: &NeedsState) -> bool {
        let value = needs.get(&self.need);
        self.above.is_none_or(|threshold| value > threshold)
            && self.below.is_none_or(|threshold| value < threshold)
    }
}

/// 一条"需求 + 标签 → 权重倍率"的偏置规则
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainBias {
    pub need: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below: Option<f64>,
    pub tag: String,
    pub factor: f64,
}

impl ChainBias {
    fn matches(&self, needs: &NeedsState) -> bool {
        let value = needs.get(&self.need);
        self.above.is_none_or(|threshold| value > threshold)
            && self.below.is_none_or(|threshold| value < threshold)
    }
}

/// 内置默认拉取规则（persona 没配 pull 时生效）
fn default_pull() -> Vec<NeedPull> {
    vec![
        NeedPull {
            need: "energy".to_string(),
            above: None,
            below: Some(0.22),
            state: "sleep".to_string(),
        },
        NeedPull {
            need: "energy".to_string(),
            above: None,
            below: Some(0.40),
            state: "relax".to_string(),
        },
        NeedPull {
            need: "hunger".to_string(),
            above: Some(0.75),
            below: None,
            state: "eat".to_string(),
        },
    ]
}

/// 内置默认权重偏置（persona 没配 chain_bias 时生效）
fn default_bias() -> Vec<ChainBias> {
    vec![
        ChainBias {
            need: "social".to_string(),
            above: Some(0.7),
            below: None,
            tag: "social".to_string(),
            factor: 1.8,
        },
        ChainBias {
            need: "energy".to_string(),
            above: None,
            below: Some(0.4),
            tag: "rest".to_string(),
            factor: 1.6,
        },
        ChainBias {
            need: "boredom".to_string(),
            above: Some(0.7),
            below: None,
            tag: "explore".to_string(),
            factor: 1.5,
        },
    ]
}

/// 关闭应用期间按"在休息"补算：回来时体力恢复、但饿了也想要人陪
const OFFLINE_REST_HOURS: f64 = 8.0;

fn needs_file(app: &AppHandle, persona_id: &str) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join(format!("needs-{persona_id}.json")))
}

/// 读取需求状态；文件缺失/损坏回退默认值。
/// 重启后按"离线期间在休息"补算（最多 8 小时），让"关掉一天再打开"有连续感。
pub fn load(app: &AppHandle, persona_id: &str, cfg: &NeedsConfig) -> NeedsState {
    let Ok(path) = needs_file(app, persona_id) else {
        return fresh_state(cfg);
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return fresh_state(cfg);
    };
    let Ok(mut state) = serde_json::from_str::<NeedsState>(&content) else {
        return fresh_state(cfg);
    };
    let now = chrono::Local::now().timestamp();
    if state.updated_unix > 0 && now > state.updated_unix {
        let hours = ((now - state.updated_unix) as f64 / 3600.0).min(OFFLINE_REST_HOURS);
        state.advance("sleep", hours, cfg);
    }
    state.updated_unix = now;
    state
}

/// 第一次运行：默认值 + persona 的 `start` 覆盖
fn fresh_state(cfg: &NeedsConfig) -> NeedsState {
    let mut state = NeedsState::default();
    for (need, value) in &cfg.start {
        state.set(need, *value);
    }
    state
}

/// 落盘需求状态（原子写）
pub fn save(app: &AppHandle, persona_id: &str, state: &NeedsState) -> Result<(), String> {
    let path = needs_file(app, persona_id)?;
    let json = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    crate::util::atomic_write(&path, &json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_falls_over_time_and_recovers_while_sleeping() {
        let cfg = NeedsConfig::default();
        let mut state = NeedsState::default();
        let start = state.energy;
        state.advance("routine", 4.0, &cfg);
        assert!(state.energy < start, "清醒 4 小时体力应下降");
        let tired = state.energy;
        state.advance("sleep", 2.0, &cfg);
        assert!(state.energy > tired, "睡 2 小时体力应回升");
        assert!(state.get("energy") <= 1.0);
    }

    #[test]
    fn hunger_rises_and_eating_relieves_it() {
        let cfg = NeedsConfig::default();
        let mut state = NeedsState::default();
        let start = state.hunger;
        state.advance("routine", 5.0, &cfg);
        assert!(state.hunger > start);
        let hungry = state.hunger;
        state.advance("eat", 0.5, &cfg);
        assert!(state.hunger < hungry, "吃饭半小时应明显解饿");
    }

    #[test]
    fn seeing_and_new_chain_lower_social_and_boredom() {
        let mut state = NeedsState {
            social: 0.9,
            boredom: 0.9,
            ..Default::default()
        };
        state.on_seen();
        state.on_new_chain();
        assert!(state.social < 0.6 && state.boredom < 0.8);
    }

    #[test]
    fn default_pull_sends_tired_character_to_rest_and_sleep() {
        let cfg = NeedsConfig::default();
        let mut state = NeedsState::default();
        assert_eq!(cfg.pull_state(&state), None, "状态正常时不该拉走");
        state.energy = 0.8;
        state.hunger = 0.9;
        assert_eq!(
            cfg.pull_state(&state),
            Some("eat".to_string()),
            "饿了先去吃"
        );
        state.hunger = 0.3;
        state.energy = 0.35;
        assert_eq!(cfg.pull_state(&state), Some("relax".to_string()));
        state.energy = 0.1;
        assert_eq!(cfg.pull_state(&state), Some("sleep".to_string()));
    }

    #[test]
    fn persona_config_overrides_rates_and_bias() {
        let mut cfg = NeedsConfig::default();
        cfg.rates.insert("energy".into(), -0.5);
        let mut state = NeedsState::default();
        let start = state.energy;
        state.advance("routine", 1.0, &cfg);
        assert!(
            (start - state.energy - 0.5).abs() < 1e-6,
            "persona 速率应生效"
        );

        cfg.chain_bias = vec![ChainBias {
            need: "hunger".into(),
            above: Some(0.5),
            below: None,
            tag: "food".into(),
            factor: 2.5,
        }];
        let state = NeedsState {
            hunger: 0.6,
            ..Default::default()
        };
        assert_eq!(cfg.chain_factor(&state, &["food".into()]), 2.5);
        assert_eq!(cfg.chain_factor(&state, &["other".into()]), 1.0);
    }
}
