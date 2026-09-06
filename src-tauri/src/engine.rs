use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        {Arc, Condvar, Mutex},
    },
    thread,
    time::Duration,
};

use chrono::Timelike;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use crate::llm::LlmMessage;

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

/// 缩放变化后广播给前端的角色显示尺寸（已乘 zoom 的最终 pixel 尺寸）
#[derive(Debug, Clone, Serialize)]
pub struct ZoomChanged {
    pub w: u32,
    pub h: u32,
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
    /// 当前角色的有效显示尺寸缓存：拖动/贴气泡时频繁读取，避免每次都克隆整个 persona
    /// 或对导入角色读磁盘做 image_dimensions。
    display_size: Arc<Mutex<(u32, u32)>>,
    /// 全局缩放偏好（含 zoom）；放在引擎侧便于 switch_persona / get_persona_config 直接用。
    prefs: Arc<Mutex<crate::prefs::Prefs>>,
    /// 每日 AI 气泡缓存（当前角色的）：气泡广播时优先取当日生成文案，取不到回退原配置。
    gen_bubbles: Arc<Mutex<crate::genbubble::GenState>>,
    /// 每日气泡生成任务是否在跑（防止重复起任务）
    pub(crate) generating: Arc<AtomicBool>,
    /// 后台节拍线程的唤醒信号：switch_persona / next_state 修改配置后 notify，
    /// 让线程立即醒来重算下一次切换时刻，避免睡到旧的 next_transition_at。
    wake_lock: Arc<Mutex<()>>,
    wake_cond: Arc<Condvar>,
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
        let prefs = crate::prefs::load_prefs(&app);
        let gen_cache = crate::genbubble::load(&app, "link");
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
            manual_state: Arc::new(Mutex::new(None)),
            display_size: Arc::new(Mutex::new(display)),
            gen_bubbles: Arc::new(Mutex::new(crate::genbubble::GenState {
                persona_id: "link".into(),
                cache: gen_cache,
            })),
            generating: Arc::new(AtomicBool::new(false)),
            prefs: Arc::new(Mutex::new(prefs)),
            wake_lock: Arc::new(Mutex::new(())),
            wake_cond: Arc::new(Condvar::new()),
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

    /// 当前角色的有效显示尺寸（缓存值），供贴气泡等高频 UI 计算使用。
    pub fn display_size(&self) -> (u32, u32) {
        *self.display_size.lock().unwrap()
    }

    /// 当前缩放偏好（克隆），供 set_zoom / get_prefs 读取。
    pub fn prefs(&self) -> crate::prefs::Prefs {
        self.prefs.lock().unwrap().clone()
    }

    // ---- 每日 AI 气泡：缓存读写与触发（见 genbubble.rs）----

    /// 广播气泡文案：优先当日 AI 生成内容，取不到回退角色自带 bubbles。
    fn bubble_text(&self, state: &str, cfg: &StateConfig) -> Option<String> {
        crate::genbubble::today_bubble(&self.gen_bubbles, state)
            .or_else(|| pick(&cfg.bubbles))
    }

    /// 某状态今天是否已有生成台词
    pub(crate) fn gen_has_today(&self, state: &str) -> bool {
        crate::genbubble::today_bubble(&self.gen_bubbles, state).is_some()
    }

    /// 某状态当天是否已标记失败（当天不再重试）
    pub(crate) fn gen_failed(&self, persona_id: &str, state: &str) -> bool {
        let g = self.gen_bubbles.lock().unwrap();
        g.persona_id == persona_id && g.cache.failed.iter().any(|s| s == state)
    }

    /// 生成历史快照（查重用）
    pub(crate) fn gen_history(&self) -> Vec<String> {
        self.gen_bubbles.lock().unwrap().cache.history.clone()
    }

    /// 记录一个状态当日生成成功：更新内存缓存并落盘（切换角色后到达的迟到结果直接丢弃）
    pub(crate) fn record_gen_bubble(&self, persona_id: &str, state: &str, text: &str) {
        let (cache, changed) = {
            let mut g = self.gen_bubbles.lock().unwrap();
            if g.persona_id != persona_id {
                return;
            }
            g.cache.date = crate::genbubble::today_str();
            g.cache.by_state.insert(state.to_string(), text.to_string());
            g.cache.history.push(text.to_string());
            let keep = g.cache.history.len().saturating_sub(crate::genbubble::HISTORY_KEEP);
            if keep > 0 {
                g.cache.history.drain(..keep);
            }
            (g.cache.clone(), true)
        };
        if changed {
            let _ = crate::genbubble::save(&self.app, persona_id, &cache);
        }
    }

    /// 记录一个状态当日生成失败（不写历史、只标记，当天回退原配置）
    pub(crate) fn record_gen_failure(&self, persona_id: &str, state: &str) {
        let (cache, changed) = {
            let mut g = self.gen_bubbles.lock().unwrap();
            if g.persona_id != persona_id || g.cache.failed.iter().any(|s| s == state) {
                return;
            }
            g.cache.date = crate::genbubble::today_str();
            g.cache.failed.push(state.to_string());
            (g.cache.clone(), true)
        };
        if changed {
            let _ = crate::genbubble::save(&self.app, persona_id, &cache);
        }
    }

    /// 是否需要（重新）生成当日气泡：缓存归属角色不符 / 日期过期 / 有状态既未生成也未标记失败
    pub(crate) fn gen_needs_refresh(&self) -> bool {
        let persona = self.persona.lock().unwrap();
        let g = self.gen_bubbles.lock().unwrap();
        if g.persona_id != persona.id {
            return true;
        }
        if g.cache.date != crate::genbubble::today_str() {
            return true;
        }
        persona.states.keys().any(|s| {
            !g.cache.by_state.contains_key(s) && !g.cache.failed.iter().any(|f| f == s)
        })
    }

    /// 设置页开关：写内存 prefs（落盘由调用方 save_prefs 完成）
    pub(crate) fn set_ai_bubbles(&self, enabled: bool) {
        self.prefs.lock().unwrap().ai_bubbles = enabled;
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
            let mut p = self.prefs.lock().unwrap();
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
        let zoom = self.prefs.lock().unwrap().zoom;
        let persona = self.persona.lock().unwrap().clone();
        let mut display = effective_display_size_zoomed(&persona, zoom);
        // 此刻未持有任何锁：取工作区（UI 查询不做锁内操作），按工作区等比适配后写回缓存。
        if let Some((mw, mh)) = work_area_content_limit(&self.app) {
            display = fit_display_size_proportional(display.0, display.1, mw, mh);
        }
        *self.display_size.lock().unwrap() = display;
    }

    /// 返回给前端用的角色配置：display_w/display_h 替换为缩放后的显示尺寸（缓存值），
    /// 其余字段保持基准语义。persona.json 本体（含 build_system_prompt 使用的
    /// system_prompt/state 字段）不受影响，因此替换 display 不影响对话提示词。
    pub fn persona_view(&self) -> PersonaConfig {
        let mut p = self.persona();
        let (w, h) = self.display_size();
        p.display_w = w;
        p.display_h = h;
        p
    }

    /// 唤醒后台节拍线程：switch_persona / next_state 在修改 persona / manual_state 后调用，
    /// 让它在新的日程/覆盖下重新计算下一次切换时刻，避免睡到旧的 next_transition_at。
    /// 只锁 wake_lock（不持有 persona / manual_state），与线程侧锁序保持一致，避免死锁。
    fn notify_wake(&self) {
        let _guard = self.wake_lock.lock().unwrap();
        self.wake_cond.notify_one();
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
        // 一并删除该角色的持久化对话历史（存在才删；删除失败不阻断主流程——避免“删角色后
        // 重新导入同名角色想起上一世对话”的意外；极端下文件残留也不影响使用）。
        if let Ok(conf) = self.app.path().app_config_dir() {
            let history_file = conf.join("history").join(format!("{id}.json"));
            if history_file.exists() {
                let _ = std::fs::remove_file(&history_file);
            }
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
        let mut sorted: Vec<(String, String)> = self
            .personas
            .lock()
            .unwrap()
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
        let persona = self
            .personas
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| format!("角色不存在: {id}"))?;
        let zoom = self.prefs.lock().unwrap().zoom;
        let mut display = effective_display_size_zoomed(&persona, zoom);
        // 切换后同样按工作区等比适配，避免新角色尺寸过大出屏。
        if let Some((mw, mh)) = work_area_content_limit(&self.app) {
            display = fit_display_size_proportional(display.0, display.1, mw, mh);
        }
        // 新角色的当前状态：切换角色本身不算“状态变化”，预先记录 last_state，
        // 避免节拍线程醒来误判为状态切换而弹出气泡（气泡逻辑与切换角色解耦）。
        let state = self.resolve_state(&persona, chrono::Local::now());
        {
            let mut cur = self.persona.lock().unwrap();
            *cur = persona.clone();
            *self.last_state.lock().unwrap() = Some(state.clone());
            *self.manual_state.lock().unwrap() = None;
            *self.display_size.lock().unwrap() = display;
        }
        // 气泡缓存整体切到新角色（内存换绑 + 磁盘加载），并视条件补跑当日生成
        {
            let cache = crate::genbubble::load(&self.app, id);
            *self.gen_bubbles.lock().unwrap() = crate::genbubble::GenState {
                persona_id: id.to_string(),
                cache,
            };
        }
        crate::genbubble::maybe_spawn_daily(app);
        // 修改完成后唤醒节拍线程，使新日程表立即生效（否则线程仍睡在旧 next_transition_at）。
        self.notify_wake();
        // 给前端的显示配置带 zoomed 尺寸，前端 applyPersona 据此零改动呈现缩放后的角色。
        let _ = app.emit("persona-changed", self.persona_view());
        let _ = app.emit(
            "state-changed",
            StateChanged {
                state,
                previous: String::new(),
            },
        );
        // 新角色缩放后尺寸可能不同：同步重设窗口并保持地板不动。
        crate::resize_persona_window(&self.app);
        Ok(())
    }

    /// 启动即广播当前角色与状态、开场气泡
    pub fn broadcast_state(&self) {
        let persona = self.persona();
        let state = self.resolve_state(&persona, chrono::Local::now());
        // 记录“该状态已广播过”，否则节拍线程醒来读到 last_state=None 会误判为状态变化，
        // 再次 emit state-changed + 开场气泡，导致开局弹两次气泡。
        *self.last_state.lock().unwrap() = Some(state.clone());
        // 带 zoomed display 给前端，前端 applyPersona 不因广播而回到基准尺寸。
        let _ = self.app.emit("persona-changed", self.persona_view());
        let _ = self.app.emit(
            "state-changed",
            StateChanged {
                state: state.clone(),
                previous: String::new(),
            },
        );
        if let Some(cfg) = persona.states.get(&state) {
            if let Some(text) = self.bubble_text(&state, cfg) {
                let _ = self.app.emit("bubble", BubbleEvent { state, text });
            }
        }
    }

    /// 后台节拍线程：睡到下一个切换时刻（manual 到期 / 时段结束 / loop 边界），
    /// 醒来检查状态，变化时广播并弹出对应气泡；单次睡眠最多 15 分钟。
    pub fn start(&self) {
        let app = self.app.clone();
        let persona = Arc::clone(&self.persona);
        let gen_bubbles = Arc::clone(&self.gen_bubbles);
        let last_state = Arc::clone(&self.last_state);
        let manual_state = Arc::clone(&self.manual_state);
        let wake_lock = Arc::clone(&self.wake_lock);
        let wake_cond = Arc::clone(&self.wake_cond);
        thread::spawn(move || loop {
            // 先算出下一次状态切换时刻，再用带超时的 Condvar 等待（+1s 缓冲，避免边界竞态）。
            // 单次睡眠不超过 15 分钟：防止时钟漂移 / DST 导致久睡不醒，醒来重算即可。
            // 持有 wake_lock 计算并进入 wait：switch_persona / next_state 的 notify
            // 必然在本线程进入等待后送达，不会丢失唤醒（唤醒后统一走下面的重算）。
            let now = chrono::Local::now();
            let wake = wake_lock.lock().unwrap();
            let next = {
                let p = persona.lock().unwrap();
                let m = manual_state.lock().unwrap();
                next_transition_at(&p, &m, &now)
            };
            let mut sleep_dur = (next - now)
                .to_std()
                .unwrap_or(Duration::from_secs(0));
            sleep_dur += Duration::from_secs(1);
            let cap = Duration::from_secs(15 * 60);
            if sleep_dur > cap {
                sleep_dur = cap;
            }
            // 超时或被 notify 唤醒都返回；释放锁后继续下面的状态检测。
            let (guard, _) = wake_cond.wait_timeout(wake, sleep_dur).unwrap();
            drop(guard);

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
                    // 优先当日 AI 生成文案，取不到回退角色自带 bubbles
                    let text = crate::genbubble::today_bubble(&gen_bubbles, &state)
                        .or_else(|| pick(&cfg.bubbles));
                    if let Some(text) = text {
                        let _ = app.emit("bubble", BubbleEvent { state, text });
                    }
                }
            }
            // 跨天/启动后补齐当日 AI 气泡（幂等，条件不满足时内部直接返回）
            crate::genbubble::maybe_spawn_daily(&app);
        });
    }

    /// 切换到“下一个状态”（按 spritesheet 行号排序循环），并手动锁定该状态。
    /// 返回切换后的状态 id。
    pub fn next_state(&self) -> String {
        // 先在作用域内读取 persona 并算出下一个状态/到期时刻，随后释放 persona 锁，
        // 再写覆盖并 notify（避免在持有 persona 锁时再锁 wake_lock）。
        let (next, expire_at, current) = {
            let persona = self.persona.lock().unwrap();
            let now = chrono::Local::now();
            let current = self.resolve_state(&persona, now);
            // states 为空（正常导入已在 petdex 侧校验，这里仅作防御）时没有可切换的下一状态，
            // 停留在当前状态，避免进入后续手动覆盖逻辑时状态为空。
            let next = next_loop_state(&persona, &current).unwrap_or_else(|| current.clone());
            // 到期时间：time 时段内 → 到该时段结束；否则按 loop 时长；
            // 都不适用（不在 loop/零时长）→ 30 分钟兜底，避免手动锁定永久卡死。
            let expire_at = next_state_expire_at(&persona.schedule, &next, now);
            (next, expire_at, current)
        };
        *self.manual_state.lock().unwrap() = Some(ManualOverride {
            state: next.clone(),
            expire_at,
        });
        *self.last_state.lock().unwrap() = Some(next.clone());
        // 新的手动覆盖带到期时间 → 唤醒线程，使该到期时刻尽早接管。
        self.notify_wake();
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
    // 返回带缩放后 display_w/h 的视图：前端 applyPersona 据此直接呈现缩放后的角色。
    engine.persona_view()
}

#[tauri::command]
pub fn get_current_state(engine: tauri::State<'_, StateEngine>) -> String {
    engine.current_state()
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

/// 历史文件路径：%APPDATA%\com.deskzen.app\history\{persona_id}.json
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
    let Ok(path) = history_file_path(&app, &persona_id) else { return Vec::new() };
    let Ok(meta) = std::fs::metadata(&path) else { return Vec::new() };
    if meta.len() > HISTORY_MAX_BYTES {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(&path) else { return Vec::new() };
    let Ok(record) = serde_json::from_str::<HistoryRecord>(&content) else { return Vec::new() };
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
    let dir = path
        .parent()
        .ok_or_else(|| "历史文件路径不合法".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("创建历史目录失败: {e}"))?;
    let tmp = dir.join(format!("{persona_id}.json.tmp"));
    let json = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
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

/// 计算右键“下个状态”被手动锁定后的到期时间：
/// - 命中的 time 时段 → 到该时段结束；
/// - 否则命中 loop 条目 → 到该条目时长；
/// - 否则（不在 loop，或条目时长恰为 0，无法给出自然到期点）→ 用 30 分钟兜底。
/// 兜底避免状态被手动锁死到切角色/重启；语义与 next_transition_at 的 30s 兜底一致（都是防久睡/锁死）。
fn next_state_expire_at(
    schedule: &ScheduleConfig,
    next: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Option<chrono::DateTime<chrono::Local>> {
    if let Some(slot) = find_active_slot(schedule, now_minutes(&now)) {
        return Some(slot_end_datetime(&now, slot));
    }
    let dur = loop_duration(schedule, next);
    if dur > 0 {
        Some(now + chrono::Duration::minutes(dur as i64))
    } else {
        Some(now + chrono::Duration::minutes(30))
    }
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
    let max_h =
        (wa.size.height as f64 / scale - (crate::SPRITE_BOTTOM + crate::TOP_MARGIN) as f64)
            .max(1.0) as u32;
    Some((max_w, max_h))
}

/// “下一个状态”：优先取循环列表中的下一个；当前不在循环列表则取循环第一个；
/// 若无循环配置，按 spritesheet 行号排序取下一个。
fn next_loop_state(persona: &PersonaConfig, current: &str) -> Option<String> {
    let entries = persona.schedule.loop_entries();
    if !entries.is_empty() {
        let idx = entries.iter().position(|e| e.state == current);
        return match idx {
            Some(i) => Some(entries[(i + 1) % entries.len()].state.clone()),
            None => Some(entries[0].state.clone()),
        };
    }
    // 无循环配置 → 按行号排序兜底
    let mut keys: Vec<&String> = persona.states.keys().collect();
    keys.sort_by_key(|k| persona.states[*k].row);
    // states 为空时 keys.len() 为 0，`(pos + 1) % keys.len()` 会除零 panic；
    // 且该调用发生在持有 persona 锁的上下文，panic 会毒化 Mutex 导致后续连环崩溃。
    // 这里直接返回 None，由调用方安全跳过（正常导入已在 petdex 侧校验 states 非空）。
    if keys.is_empty() {
        return None;
    }
    let pos = keys.iter().position(|k| *k == current).unwrap_or(0);
    Some(keys[(pos + 1) % keys.len()].clone())
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
/// manual 到期、当前 time 时段结束、下一个 loop 边界；都不可得则 now + 30s 兜底。
fn next_transition_at(
    persona: &PersonaConfig,
    manual: &Option<ManualOverride>,
    now: &chrono::DateTime<chrono::Local>,
) -> chrono::DateTime<chrono::Local> {
    let mut candidates: Vec<chrono::DateTime<chrono::Local>> = Vec::new();

    // manual 到期
    if let Some(ov) = manual {
        if let Some(exp) = ov.expire_at {
            if *now < exp {
                candidates.push(exp);
            }
        }
    }
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
        assert_eq!(
            next_loop_state(&p, &entries[0].state),
            Some(entries[1].state.clone())
        );
        assert_eq!(
            next_loop_state(&p, &entries[entries.len() - 1].state),
            Some(entries[0].state.clone())
        );
        // 当前不在循环列表（如未知状态）→ 取循环第一个
        assert_eq!(
            next_loop_state(&p, "unknown"),
            Some(entries[0].state.clone())
        );
    }

    #[test]
    fn next_loop_state_empty_states_is_none() {
        // 空 states 且无循环配置时会落入 keys 兜底分支，历史上 `(pos+1) % keys.len()`
        // 会除零 panic；现在应安全返回 None。
        let mut p = link();
        p.states.clear();
        p.schedule.r#loop.clear();
        assert_eq!(next_loop_state(&p, "Awake"), None);
    }

    #[test]
    fn next_state_expire_zero_duration_uses_30min_fallback() {
        // 命中零时长 loop 条目时没有自然到期点，历史上 expire_at=None 会把该状态永久锁死；
        // 现在应回退为 30 分钟兜底，而不是 None。
        let mut p = link();
        p.schedule.time.clear();
        let state = p.schedule.r#loop.first().expect("link 应有循环").state.clone();
        p.schedule.r#loop = vec![LoopEntry {
            state: state.clone(),
            duration: 0,
        }];
        let now = chrono::Local::now();
        let exp = next_state_expire_at(&p.schedule, &state, now)
            .expect("零时长条目应回退 30 分钟，而非 None");
        assert_eq!((exp - now).num_minutes(), 30);
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

    #[test]
    fn zoomed_display_scales_base_and_min_one() {
        let p = link();
        // 2.0 翻倍
        assert_eq!(
            effective_display_size_zoomed(&p, 2.0),
            (p.display_w * 2, p.display_h * 2)
        );
        // 0.5 减半（125 * 0.5 = 62.5，四舍五入为 63）
        assert_eq!(
            effective_display_size_zoomed(&p, 0.5),
            (58, 63)
        );
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
        assert_eq!(fit_display_size_proportional(400, 400, 200, 200), (200, 200));
        // 上限为 0：不除零，按 max(1) 兜底
        assert_eq!(fit_display_size_proportional(1, 1, 0, 0), (1, 1));
        // 极小值下限：等比缩后高度约 0.3px，须钳制到 1px，避免缩成 0
        assert_eq!(fit_display_size_proportional(1000, 1, 300, 300), (300, 1));
    }

    #[test]
    fn history_id_validation() {
        assert!(is_valid_history_id("link"));
        assert!(is_valid_history_id("petdex-doraemon"));
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

    #[test]
    fn next_transition_uses_loop_boundary_in_time_slot() {
        let p = link();
        // 12:30 落在 [12:00-13:00] 时段：loop 边界(now+10=12:40)早于时段结束(13:00)
        let now = local_at(12, 30);
        assert_eq!(
            next_transition_at(&p, &None, &now),
            now + chrono::Duration::minutes(10)
        );
    }

    #[test]
    fn next_transition_uses_time_slot_end() {
        let p = link();
        // 14:29 落在 [13:00-14:30] 时段：时段结束(14:30)早于 next loop 边界(14:40)
        let now = local_at(14, 29);
        assert_eq!(next_transition_at(&p, &None, &now), local_at(14, 30));
    }

    #[test]
    fn next_transition_uses_manual_expiry() {
        let p = link();
        // 09:00 无时段：manual 到期(09:05)最早，早于 next loop 边界(09:20)
        let now = local_at(9, 0);
        let manual = Some(ManualOverride {
            state: "idle".into(),
            expire_at: Some(local_at(9, 5)),
        });
        assert_eq!(
            next_transition_at(&p, &manual, &now),
            local_at(9, 5)
        );
    }

    #[test]
    fn next_transition_crosses_midnight_boundary() {
        let p = link();
        // 23:50 在跨午夜 sleep 时段内：loop 边界(now+10=次日 00:00)早于时段结束(次日 08:00)
        let now = local_at(23, 50);
        assert_eq!(
            next_transition_at(&p, &None, &now),
            now + chrono::Duration::minutes(10)
        );
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
            next_transition_at(&p, &None, &now),
            now + chrono::Duration::seconds(30)
        );
    }
}
