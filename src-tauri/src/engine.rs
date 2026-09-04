use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        {Arc, Mutex},
    },
    thread,
    time::Duration,
};

use chrono::Timelike;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

/// 一个角色的完整配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PersonaConfig {
    pub id: String,
    pub name: String,
    /// spritesheet 前端资源路径
    pub spritesheet: String,
    /// spritesheet 总列数（整张图的列数）
    pub cols: u32,
    /// spritesheet 总行数（每个状态一行）
    pub rows: u32,
    /// 是否像素画（决定放大渲染方式）
    pub pixel_art: bool,
    /// 角色在窗口中的显示尺寸
    #[serde(default)]
    pub display_w: u32,
    #[serde(default)]
    pub display_h: u32,
    pub system_prompt: SystemPromptConfig,
    pub states: HashMap<String, StateConfig>,
    pub schedule: ScheduleConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemPromptConfig {
    pub definition: String,
    pub reply_style: String,
    pub state_guidelines: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StateConfig {
    /// 状态的中文显示名（对话窗头部）
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

/// 手动指定的状态覆盖（右键“下个状态”）
#[derive(Debug, Clone)]
pub struct ManualOverride {
    pub state: String,
    /// 覆盖到期时间；到点后自动恢复为“日程自动计算”。None 表示持续到切换角色/重启。
    pub expire_at: Option<chrono::DateTime<chrono::Local>>,
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

/// 内置角色：id -> 配置文件（编译期内嵌，运行时切换）
const EMBEDDED_PERSONAS: &[(&str, &str)] = &[
    ("link", include_str!("../../resources/characters/link/persona.json")),
];

/// 生活状态引擎：根据本地时间与当前 persona 作息表计算状态，
/// 变化时向所有窗口广播事件。只依赖 Rust 进程，不依赖 WebView 存活。
pub struct StateEngine {
    app: AppHandle,
    personas: Arc<Mutex<HashMap<String, PersonaConfig>>>,
    persona: Arc<Mutex<PersonaConfig>>,
    last_state: Arc<Mutex<Option<String>>>,
    /// 手动指定的状态覆盖（右键“下个状态”）；到期后自动恢复日程计算
    manual_state: Arc<Mutex<Option<ManualOverride>>>,
}

impl StateEngine {
    pub fn new(app: AppHandle) -> Self {
        let mut personas = HashMap::new();
        for (id, json) in EMBEDDED_PERSONAS {
            let cfg: PersonaConfig =
                serde_json::from_str(json).expect("persona 配置解析失败");
            personas.insert((*id).to_string(), cfg);
        }
        // 加载用户通过 petdex 导入的角色（持久化在用户数据目录）
        if let Ok(chars_dir) = Self::characters_dir_for(&app) {
            if let Ok(entries) = std::fs::read_dir(&chars_dir) {
                for entry in entries.flatten() {
                    let cfg_path = entry.path().join("persona.json");
                    let Ok(json) = std::fs::read_to_string(&cfg_path) else {
                        continue;
                    };
                    if let Ok(cfg) = serde_json::from_str::<PersonaConfig>(&json) {
                        personas.insert(cfg.id.clone(), cfg);
                    }
                }
            }
        }
        let persona = personas
            .get("link")
            .cloned()
            .expect("缺少默认角色 link");
        Self {
            app,
            personas: Arc::new(Mutex::new(personas)),
            persona: Arc::new(Mutex::new(persona)),
            last_state: Arc::new(Mutex::new(None)),
            manual_state: Arc::new(Mutex::new(None)),
        }
    }

    /// 当前状态：若手动指定则用之，否则按本地时间实时计算
    pub fn current_state(&self) -> String {
        let persona = self.persona.lock().unwrap();
        self.resolve_state(&persona, chrono::Local::now())
    }

    /// 结合手动覆盖与日程计算得出最终状态
    fn resolve_state(
        &self,
        persona: &PersonaConfig,
        now: chrono::DateTime<chrono::Local>,
    ) -> String {
        let mut guard = self.manual_state.lock().unwrap();
        if let Some(ov) = guard.as_ref() {
            if let Some(exp) = ov.expire_at {
                if now >= exp {
                    *guard = None;
                }
            }
        }
        effective_state(&*guard, persona, &now)
    }

    /// 当前角色配置（克隆）
    pub fn persona(&self) -> PersonaConfig {
        self.persona.lock().unwrap().clone()
    }

    pub fn persona_id(&self) -> String {
        self.persona.lock().unwrap().id.clone()
    }

    /// 用户导入角色的持久化目录（%APPDATA%\com.deskzen.app\characters\）
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

    /// 运行时注册一个角色（petdex 导入后调用；已存在则覆盖）
    pub fn register_persona(&self, cfg: PersonaConfig) {
        self.personas.lock().unwrap().insert(cfg.id.clone(), cfg);
    }

    /// 删除导入角色：先删除磁盘目录，再从注册表移除。
    /// 内置角色（如 link）不允许删除。返回被删除角色的显示名。
    pub fn remove_persona(&self, id: &str) -> Result<String, String> {
        if !(id.starts_with("petdex-") || id.starts_with("local-")) {
            return Err("内置角色不可删除".into());
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err("角色 id 不合法".into());
        }
        let dir = self.characters_dir()?.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| format!("删除角色文件失败: {e}"))?;
        }
        let name = self
            .personas
            .lock()
            .unwrap()
            .remove(id)
            .map(|p| p.name)
            .ok_or_else(|| format!("角色不存在: {id}"))?;
        Ok(name)
    }

    /// 所有可切换角色 (id, 显示名)
    pub fn list_personas(&self) -> Vec<(String, String)> {
        self.personas
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| (id.clone(), p.name.clone()))
            .collect()
    }

    /// 运行时切换角色：更新配置与作息表，并广播事件让前端重新渲染
    pub fn switch_persona(&self, app: &AppHandle, id: &str) -> Result<(), String> {
        let persona = self
            .personas
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("角色不存在: {id}"))?;
        {
            let mut cur = self.persona.lock().unwrap();
            *cur = persona.clone();
            *self.last_state.lock().unwrap() = None;
            *self.manual_state.lock().unwrap() = None;
        }
        let state = self.resolve_state(&persona, chrono::Local::now());
        let _ = app.emit("persona-changed", persona);
        let _ = app.emit(
            "state-changed",
            StateChanged {
                state,
                previous: String::new(),
            },
        );
        Ok(())
    }

    /// 启动即广播当前角色与状态、开场气泡
    pub fn broadcast_state(&self) {
        let persona = self.persona();
        let state = self.resolve_state(&persona, chrono::Local::now());
        let _ = self.app.emit("persona-changed", persona.clone());
        let _ = self.app.emit(
            "state-changed",
            StateChanged {
                state: state.clone(),
                previous: String::new(),
            },
        );
        if let Some(cfg) = persona.states.get(&state) {
            if let Some(text) = pick(&cfg.bubbles) {
                let _ = self.app.emit("bubble", BubbleEvent { state, text });
            }
        }
    }

    /// 后台节拍线程：每 30 秒检查一次状态，变化时广播并弹出对应气泡
    pub fn start(&self) {
        let app = self.app.clone();
        let persona = Arc::clone(&self.persona);
        let last_state = Arc::clone(&self.last_state);
        let manual_state = Arc::clone(&self.manual_state);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(30));
            let now = chrono::Local::now();
            let (state, persona) = {
                let p = persona.lock().unwrap();
                let mut m = manual_state.lock().unwrap();
                if let Some(ov) = m.as_ref() {
                    if let Some(exp) = ov.expire_at {
                        if now >= exp {
                            *m = None;
                        }
                    }
                }
                let s = effective_state(&*m, &p, &now);
                (s, p.clone())
            };
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

    /// 切换到“下一个状态”（按 spritesheet 行号排序循环），并手动锁定该状态。
    /// 返回切换后的状态 id。
    pub fn next_state(&self) -> String {
        let persona = self.persona.lock().unwrap();
        let now = chrono::Local::now();
        let current = self.resolve_state(&persona, now);
        let next = next_loop_state(&persona, &current);
        // 到期时间：time 时段内 → 到该时段结束；否则若启用循环 → 到一个 slot；
        // 都没有 → 无到期（手动锁定）。
        let mins = now_minutes(&now);
        let expire_at = if let Some(slot) = find_active_slot(&persona.schedule, mins) {
            Some(slot_end_datetime(&now, slot))
        } else {
            let dur = loop_duration(&persona.schedule, &next);
            if dur > 0 {
                Some(now + chrono::Duration::minutes(dur as i64))
            } else {
                None
            }
        };
        *self.manual_state.lock().unwrap() = Some(ManualOverride {
            state: next.clone(),
            expire_at,
        });
        *self.last_state.lock().unwrap() = Some(next.clone());
        let _ = self.app.emit(
            "state-changed",
            StateChanged {
                state: next.clone(),
                previous: current,
            },
        );
        next
    }
}

#[tauri::command]
pub fn get_persona_config(engine: tauri::State<'_, StateEngine>) -> PersonaConfig {
    engine.persona()
}

#[tauri::command]
pub fn get_current_state(engine: tauri::State<'_, StateEngine>) -> String {
    engine.current_state()
}

fn now_minutes(now: &chrono::DateTime<chrono::Local>) -> u32 {
    now.hour() as u32 * 60 + now.minute() as u32
}

/// 结合手动覆盖与日程自动计算
fn effective_state(
    manual: &Option<ManualOverride>,
    persona: &PersonaConfig,
    now: &chrono::DateTime<chrono::Local>,
) -> String {
    if let Some(ov) = manual {
        if let Some(exp) = ov.expire_at {
            if *now >= exp {
                return automatic_state(persona, now_minutes(now));
            }
        }
        return ov.state.clone();
    }
    automatic_state(persona, now_minutes(now))
}

/// 查找当前生效的 time 时段（支持跨午夜）
fn find_active_slot<'a>(schedule: &'a ScheduleConfig, mins: u32) -> Option<&'a TimeSlot> {
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

/// 某循环状态在 loop 里的时长（分钟）；不在循环里或未配置则返回 0
fn loop_duration(schedule: &ScheduleConfig, state: &str) -> u32 {
    schedule
        .loop_entries()
        .iter()
        .find(|e| e.state == state)
        .map(|e| e.duration)
        .unwrap_or(0)
}

/// 自动状态：time 时段优先，否则循环，最后兜底
fn automatic_state(persona: &PersonaConfig, mins: u32) -> String {
    if let Some(slot) = find_active_slot(&persona.schedule, mins) {
        return slot.state.clone();
    }
    if let Some(s) = loop_state_at(&persona.schedule, mins) {
        return s;
    }
    // 兜底：按 spritesheet 行号取第一个状态（与 next_loop_state 的排序一致）；
    // 仅当角色完全没有定义状态时才使用硬编码值。
    let mut keys: Vec<&String> = persona.states.keys().collect();
    keys.sort_by_key(|k| persona.states[*k].row);
    keys.first()
        .map(|k| (*k).clone())
        .unwrap_or_else(|| "Awake".into())
}

/// 计算角色的有效显示尺寸：优先用配置文件；为 0（未配置）时按精灵图实际尺寸 ÷ cols/rows 计算。
pub fn effective_display_size(persona: &PersonaConfig) -> (u32, u32) {
    let (w, h) = (persona.display_w, persona.display_h);
    if w > 0 && h > 0 {
        return (w, h);
    }
    // 内置角色 spritesheet 是站点路径(/…)，无法直接读文件；导入角色是磁盘路径
    if !persona.spritesheet.starts_with('/') {
        if let Ok(p) = Path::new(&persona.spritesheet).canonicalize() {
            if let Ok((sw, sh)) = image::image_dimensions(&p) {
                let cols = persona.cols.max(1) as u32;
                let rows = persona.rows.max(1) as u32;
                let cw = ((sw as f64 / cols as f64).round() as u32).max(1);
                let ch = ((sh as f64 / rows as f64).round() as u32).max(1);
                return (cw, ch);
            }
        }
    }
    (w.max(1), h.max(1))
}

/// “下一个状态”：优先取循环列表中的下一个；当前不在循环列表则取循环第一个；
/// 若无循环配置，按 spritesheet 行号排序取下一个。
fn next_loop_state(persona: &PersonaConfig, current: &str) -> String {
    let entries = persona.schedule.loop_entries();
    if !entries.is_empty() {
        let idx = entries.iter().position(|e| e.state == current);
        return match idx {
            Some(i) => entries[(i + 1) % entries.len()].state.clone(),
            None => entries[0].state.clone(),
        };
    }
    // 无循环配置 → 按行号排序兜底
    let mut keys: Vec<&String> = persona.states.keys().collect();
    keys.sort_by_key(|k| persona.states[*k].row);
    let pos = keys.iter().position(|k| *k == current).unwrap_or(0);
    keys[(pos + 1) % keys.len()].clone()
}

/// 计算 time 时段的结束时刻（用于手动覆盖到期）
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

/// xorshift64 状态（惰性以纳秒时间做种子），避免引入 rand 依赖
static RNG_STATE: AtomicU64 = AtomicU64::new(0);

/// 简易伪随机：推进 xorshift64 状态后从文本池取一条，减少连续命中同一文本（PRNG，非去重）
fn pick(list: &[String]) -> Option<String> {
    if list.is_empty() {
        return None;
    }
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
    Some(list[(s as usize) % list.len()].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> PersonaConfig {
        serde_json::from_str(include_str!(
            "../../resources/characters/link/persona.json"
        ))
        .expect("内置 persona.json 解析失败")
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
        assert_eq!(automatic_state(&p, mid), slot.state);
    }

    #[test]
    fn next_loop_state_cycles() {
        let p = link();
        let entries = p.schedule.loop_entries();
        assert_eq!(next_loop_state(&p, &entries[0].state), entries[1].state);
        assert_eq!(
            next_loop_state(&p, &entries[entries.len() - 1].state),
            entries[0].state
        );
        // 当前不在循环列表（如未知状态）→ 取循环第一个
        assert_eq!(next_loop_state(&p, "unknown"), entries[0].state);
    }

    #[test]
    fn loop_state_at_follows_durations() {
        let p = link();
        let entries = p.schedule.loop_entries();
        // 第 0 分钟 → 第一个状态；第 duration 分钟 → 第二个；首个总时长处 → 回到第一个
        assert_eq!(loop_state_at(&p.schedule, 0), Some(entries[0].state.clone()));
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
        assert!(p.display_w > 0 && p.display_h > 0, "link 应显式配置显示尺寸");
        assert_eq!(effective_display_size(&p), (p.display_w, p.display_h));
    }

    #[test]
    fn effective_display_computes_from_sprite() {
        let mut p = link();
        p.display_w = 0;
        p.display_h = 0;
        // 用仓库内真实 spritesheet 路径（文件系统可读）：3840/20=192, 1248/6=208
        let sprite = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../resources/characters/link/spritesheet.webp");
        p.spritesheet = sprite.to_string_lossy().into_owned();
        assert_eq!(effective_display_size(&p), (192, 208));
    }
}
