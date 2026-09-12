mod characters;
mod engine;
mod genbubble;
mod llm;
mod playback;
mod prefs;
mod screen;
mod util;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use tauri::{
    menu::{Menu, MenuEvent, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder,
};

/// 角色精灵在角色窗口内的底距（与 styles.css `.character { bottom: 24px }` 同步）
pub(crate) const SPRITE_BOTTOM: i32 = 24;
/// 角色窗口显示余量：水平每侧 ≥30px（气泡可能比精灵宽，需留横向空间）、顶部 ≥96px
/// （气泡显示空间：需容纳 3 行气泡 ≈73px + 与精灵的间隙 9px，两行气泡在 50px 时代会被窗口顶部截断）。
pub(crate) const H_MARGIN: i32 = 30;
pub(crate) const TOP_MARGIN: i32 = 96;
/// 窗口最小逻辑尺寸：与 tauri.conf.json 初始 240×280 一致，保证基准缩放下气泡不溢出、尺寸稳定。
const MIN_WINDOW_W: i32 = 240;
const MIN_WINDOW_H: i32 = 280;

/// 应用级共享状态
pub struct AppState {
    /// 是否处于整窗点击穿透模式
    pub passthrough: Mutex<bool>,
    /// 前端是否已就绪：`frontend_ready` 被调用过一次后置真，防止重复广播开场事件
    pub frontend_ready: AtomicBool,
    /// 托盘菜单里的“显示/隐藏角色”项，用于动态更新文案
    pub persona_menu_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// 托盘“更换角色”子菜单项，id -> 菜单项
    pub persona_items: Mutex<HashMap<String, MenuItem<tauri::Wry>>>,
    /// 托盘“更换角色”子菜单句柄，导入新角色后动态追加菜单项
    pub persona_submenu: Mutex<Option<Submenu<tauri::Wry>>>,
}

#[derive(serde::Serialize)]
struct ChatReply {
    reply: String,
}

#[derive(serde::Serialize)]
struct PersonaInfo {
    id: String,
    name: String,
}

/// 应用 identifier 从旧的 `com.deskzen.app` 改为 `com.deskzen.desktop` 后，`%APPDATA%`
/// 下的数据目录随之改变。首次用新版本启动时把旧目录整体复制过来，保证已导入角色、
/// 对话历史、llm.json、prefs.json 不丢；仅当新目录不存在时执行（幂等），失败只记日志不阻塞启动。
fn migrate_legacy_data_dir(app: &AppHandle) {
    let Ok(new_dir) = app.path().app_config_dir() else {
        return;
    };
    if new_dir.exists() {
        return;
    }
    let Some(parent) = new_dir.parent() else {
        return;
    };
    let legacy = parent.join("com.deskzen.app");
    if !legacy.is_dir() {
        return;
    }
    if let Err(error) = crate::util::copy_dir_recursive(&legacy, &new_dir) {
        eprintln!(
            "迁移旧数据目录失败（{} -> {}）：{error}",
            legacy.display(),
            new_dir.display()
        );
    }
}

pub fn run() {
    tauri::Builder::default()
        // 单实例插件必须最先注册：第二次启动不再开新进程，而是唤起已有实例的角色窗口。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("persona") {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            passthrough: Mutex::new(false),
            frontend_ready: AtomicBool::new(false),
            persona_menu_item: Mutex::new(None),
            persona_items: Mutex::new(HashMap::new()),
            persona_submenu: Mutex::new(None),
        })
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            // 必须在读取 prefs / 角色 / 对话历史之前完成旧数据目录迁移
            migrate_legacy_data_dir(app.handle());
            let engine = engine::StateEngine::new(app.handle().clone());
            app.manage(engine);
            app.state::<engine::StateEngine>().start();

            setup_tray(app.handle())?;

            // 按持久化的 zoom 调整窗口尺寸，使重启后缩放立即生效（精灵尺寸由前端按 zoomed 值渲染）。
            resize_persona_window(app.handle());

            // 启动即检查并补跑当日 AI 气泡生成（开关关闭/未配 Key/已生成完时内部直接返回）
            genbubble::maybe_spawn_for_state(app.handle());

            // 等前端就绪后再显示，避免透明窗口启动白屏闪烁
            let persona = app.get_webview_window("persona").unwrap();
            // 默认停在主显示器右下角（给任务栏留出空间），避免遮挡居中的对话窗口
            if let Some(monitor) = app.primary_monitor()? {
                let wa = monitor.work_area();
                if let Ok(p_size) = persona.outer_size() {
                    let mut x = wa.position.x + wa.size.width as i32 - p_size.width as i32 - 40;
                    let mut y = wa.position.y + wa.size.height as i32 - p_size.height as i32 - 70;
                    // 钳制到主显示器工作区，避免窗口（尤其高 zoom 放大后）大于屏幕时为负坐标。
                    let min_x = wa.position.x;
                    let min_y = wa.position.y;
                    let max_x = (min_x + wa.size.width as i32 - p_size.width as i32).max(min_x);
                    let max_y = (min_y + wa.size.height as i32 - p_size.height as i32).max(min_y);
                    x = x.clamp(min_x, max_x);
                    y = y.clamp(min_y, max_y);
                    let _ = persona.set_position(PhysicalPosition::new(x, y));
                }
            }
            persona.show()?;

            // 角色被拖动时，让对话窗跟着移动，保持“面对面聊天”的感觉
            let app_handle = app.handle().clone();
            persona.on_window_event(move |event| {
                if let tauri::WindowEvent::Moved(_) = event {
                    if let Some(chat) = app_handle.get_webview_window("chat") {
                        if chat.is_visible().unwrap_or(false) {
                            place_chat_bubble(&app_handle);
                        }
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            engine::get_persona_config,
            engine::get_current_state,
            engine::load_chat_history,
            engine::save_chat_history,
            chat_send,
            open_chat,
            open_settings,
            switch_persona,
            list_personas,
            frontend_ready,
            characters::import_local_character,
            characters::delete_persona,
            get_llm_config,
            save_llm_config,
            get_passthrough,
            set_passthrough,
            get_prefs,
            set_zoom,
            set_ai_bubbles,
            get_current_persona_id,
            show_persona_menu,
            capture_screen,
            reposition_chat,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("DeskZen 启动失败");
}

/// 角色右键菜单：下个状态 / 隐藏
#[tauri::command]
fn show_persona_menu(app: AppHandle) -> Result<(), String> {
    // 注意：id 不能以 "persona_" 开头，否则会被托盘菜单当成“切换角色”解析
    let next = MenuItem::with_id(&app, "ctx_next_state", "下个状态", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let hide = MenuItem::with_id(&app, "ctx_hide", "隐藏", true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let menu = Menu::with_items(&app, &[&next, &hide]).map_err(|e| e.to_string())?;
    let persona = app.get_webview_window("persona").ok_or("找不到角色窗口")?;
    persona.popup_menu(&menu).map_err(|e| e.to_string())
}

/// 菜单项点击分发（应用级全局处理角色右键菜单事件）
fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "ctx_next_state" => {
            app.state::<engine::StateEngine>().next_state();
        }
        "ctx_hide" => {
            if let Some(win) = app.get_webview_window("persona") {
                let _ = win.hide();
            }
            update_persona_menu_label(app);
        }
        _ => {}
    }
}

/// 点击角色/气泡后打开（或聚焦）对话窗口
#[tauri::command]
async fn open_chat(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("chat") {
        place_chat_bubble(&app);
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "chat", WebviewUrl::App("chat.html".into()))
        .title("DeskZen · 对话")
        .inner_size(340.0, 220.0)
        .min_inner_size(300.0, 140.0)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    place_chat_bubble(&app);
    if let Some(win) = app.get_webview_window("chat") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

/// 把对话气泡摆到角色上方/左上方（尖角指向角色）；上方放不下则放到角色右侧。
/// 限制在显示器工作区内。
fn place_chat_bubble(app: &AppHandle) {
    let Some(persona) = app.get_webview_window("persona") else {
        return;
    };
    let Some(chat) = app.get_webview_window("chat") else {
        return;
    };
    let Ok(p_pos) = persona.outer_position() else {
        return;
    };
    let Ok(p_size) = persona.outer_size() else {
        return;
    };
    let Ok(c_size) = chat.outer_size() else {
        return;
    };
    let gap: i32 = 6;
    // 角色精灵头顶的屏幕纵坐标（底距 24 + 精灵高 display_h）
    let display_h = app.state::<engine::StateEngine>().display_size().1 as i32;
    let char_top = p_pos.y + p_size.height as i32 - SPRITE_BOTTOM - display_h;
    // 默认放在角色上方，水平居中；气泡底贴近精灵头顶（重叠角色窗上半透明区）
    let mut x = p_pos.x + (p_size.width as i32 - c_size.width as i32) / 2;
    let mut y = char_top - c_size.height as i32 - gap;
    if let Ok(Some(monitor)) = persona.current_monitor() {
        let wa = monitor.work_area();
        let min_x = wa.position.x;
        let min_y = wa.position.y;
        let max_x = min_x + wa.size.width as i32;
        let max_y = min_y + wa.size.height as i32;
        // 上方放不下 → 放到角色右侧（垂直对齐角色中心）
        if y < min_y {
            x = p_pos.x + p_size.width as i32 + gap;
            y = (p_pos.y + p_size.height as i32 / 2 - c_size.height as i32 / 2)
                .clamp(min_y, max_y - c_size.height as i32);
        }
        x = x.clamp(min_x, max_x - c_size.width as i32);
        y = y.clamp(min_y, max_y - c_size.height as i32);
    }
    let _ = chat.set_position(tauri::PhysicalPosition::new(x, y));
}

/// 供前端在气泡高度自适应后调用：重新贴到角色附近
#[tauri::command]
fn reposition_chat(app: AppHandle) {
    place_chat_bubble(&app);
}

/// 按当前角色「缩放后」的显示尺寸重设 persona 窗口，并保持精灵“地板”（底部中心点）不动：
/// 窗口底部 y 固定（new_y = old_y + (old_h - new_h)），水平方向保持中心对齐（new_x = old_x + (old_w - new_w)/2），
/// 使角色在缩放时看起来只是原地变大/变小，而不会水平漂移。窗口随尺寸增大上下、左右对称扩展。
/// 坐标/尺寸统一用 Physical 像素，避免与 Logical 混用导致位置偏移。
pub(crate) fn resize_persona_window(app: &AppHandle) {
    let Some(persona) = app.get_webview_window("persona") else {
        return;
    };
    let (display_w, display_h) = app.state::<engine::StateEngine>().display_size();
    let scale = persona.scale_factor().unwrap_or(1.0);
    // 展示与气泡都在 CSS（逻辑）像素里；先算逻辑窗口尺寸，再乘 scale 转物理尺寸交给 set_size。
    let win_w_log = ((display_w as i32 + 2 * H_MARGIN).max(MIN_WINDOW_W)) as f64;
    let win_h_log = ((display_h as i32 + SPRITE_BOTTOM + TOP_MARGIN).max(MIN_WINDOW_H)) as f64;
    let new_w = (win_w_log * scale).round() as i32;
    let new_h = (win_h_log * scale).round() as i32;
    if let (Ok(old_pos), Ok(old_size)) = (persona.outer_position(), persona.outer_size()) {
        let mut new_x = old_pos.x + (old_size.width as i32 - new_w) / 2;
        let mut new_y = old_pos.y + (old_size.height as i32 - new_h);
        // 底边锚定、水平居中的语义在未超屏时保持不变；把最终位置钳制到当前显示器工作区，避免窗口出屏。
        if let Ok(Some(monitor)) = persona.current_monitor() {
            let wa = monitor.work_area();
            let min_x = wa.position.x;
            let min_y = wa.position.y;
            // 窗口比工作区还大时 max < min，用 max(min) 保证 clamp 区间合法（贴 wa 左/上边）。
            let max_x = (min_x + wa.size.width as i32 - new_w).max(min_x);
            let max_y = (min_y + wa.size.height as i32 - new_h).max(min_y);
            new_x = new_x.clamp(min_x, max_x);
            new_y = new_y.clamp(min_y, max_y);
        }
        let _ = persona.set_position(PhysicalPosition::new(new_x, new_y));
    }
    let _ = persona.set_size(PhysicalSize::new(new_w, new_h));
    // 对话窗若可见，按新尺寸重新贴附到角色附近。
    if let Some(chat) = app.get_webview_window("chat") {
        if chat.is_visible().unwrap_or(false) {
            place_chat_bubble(app);
        }
    }
}

/// 打开（或聚焦）设置窗口
#[tauri::command]
async fn open_settings(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings.html".into()))
        .title("DeskZen · 设置")
        // 侧边栏收窄到 76px 后内容区相应变窄，默认宽度从 560 收到 480 以匹配表单内容（min_inner_size 420 不动）。
        .inner_size(480.0, 620.0)
        .min_inner_size(420.0, 500.0)
        .center()
        // 先隐藏创建，等前端就绪后再显示，避免 WebView2 未渲染时闪现空白窗口。
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 切换当前角色（托盘“更换角色”菜单 / 后续设置界面也可调用）
#[tauri::command]
fn switch_persona(
    app: AppHandle,
    id: String,
    engine: tauri::State<'_, engine::StateEngine>,
) -> Result<(), String> {
    engine.switch_persona(&app, &id)?;
    // 设置页等 IPC 入口切换后也要同步托盘“更换角色”子菜单的 ✓ 标记（与托盘路径行为一致）
    update_persona_menu_labels(&app);
    Ok(())
}

/// 列出全部可切换角色（设置界面据此展示已导入角色）
#[tauri::command]
fn list_personas(engine: tauri::State<'_, engine::StateEngine>) -> Vec<PersonaInfo> {
    engine
        .list_personas()
        .into_iter()
        .map(|(id, name)| PersonaInfo { id, name })
        .collect()
}

/// 当前角色 id（设置页角色列表高亮“当前”行用）
#[tauri::command]
fn get_current_persona_id(engine: tauri::State<'_, engine::StateEngine>) -> String {
    engine.persona_id()
}

#[derive(serde::Serialize)]
struct LlmConfigView {
    base_url: String,
    model: String,
    api_key: String,
    temperature: f32,
    max_tokens: u32,
}

/// API Key 打码后回传前端：保留前 3 后 4 位，中间以 **** 填充；
/// 过短（≤8 位）时整体显示为 ****。空 Key 原样返回。
fn mask_api_key(key: &str) -> String {
    let n = key.chars().count();
    if n == 0 {
        return String::new();
    }
    if n <= 8 {
        return "****".into();
    }
    let chars: Vec<char> = key.chars().collect();
    format!(
        "{}****{}",
        chars[..3].iter().collect::<String>(),
        chars[n - 4..].iter().collect::<String>()
    )
}

/// 决定保存的 API Key：回传值恰好等于「现有 Key 的打码结果」且现有 Key 非空时，说明用户
/// 未改动，沿用现有 Key；否则视为用户新输入（真实 Key 含 * 时也能被正确识别为新值保存）。
fn decide_api_key(incoming: String, existing: &str) -> String {
    if !existing.is_empty() && incoming == mask_api_key(existing) {
        existing.to_string()
    } else {
        incoming
    }
}

#[tauri::command]
fn get_llm_config(app: AppHandle) -> LlmConfigView {
    let cfg = llm::load_config(&app);
    LlmConfigView {
        base_url: cfg.base_url,
        model: cfg.model,
        api_key: mask_api_key(&cfg.api_key),
        temperature: cfg.temperature,
        max_tokens: cfg.max_tokens,
    }
}

#[tauri::command]
fn save_llm_config(
    app: AppHandle,
    base_url: String,
    model: String,
    api_key: String,
    temperature: f32,
    max_tokens: u32,
) -> Result<(), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // 前端回传的是打码值（用户未重新输入）→ 沿用已保存的 Key。用“等于打码结果”精确比较，
    // 而不是“含 *”：真实 Key 恰好含 * 时用 contains 会被误判为打码值而错误沿用旧 Key。
    let existing = llm::load_config(&app).api_key;
    let api_key = decide_api_key(api_key, &existing);
    let cfg = llm::LlmConfig {
        base_url,
        model,
        api_key,
        temperature: llm::clamp_temperature(temperature),
        max_tokens: llm::clamp_max_tokens(max_tokens),
    };
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("llm.json"), json).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_passthrough(state: tauri::State<'_, AppState>) -> bool {
    *crate::util::lock(&state.passthrough)
}

#[tauri::command]
fn set_passthrough(app: AppHandle, enabled: bool, state: tauri::State<'_, AppState>) {
    *crate::util::lock(&state.passthrough) = enabled;
    if let Some(win) = app.get_webview_window("persona") {
        let _ = win.set_ignore_cursor_events(enabled);
    }
}

/// 读取全局缩放偏好（设置页回显当前档位）
#[tauri::command]
fn get_prefs(engine: tauri::State<'_, engine::StateEngine>) -> prefs::Prefs {
    engine.prefs()
}

/// 设置全局缩放：clamp 校验、写盘、刷新显示尺寸缓存、重设窗口并广播新尺寸。
/// 缩放作用于所有角色（全局），不改 persona.json 的基准 display_w/h。
#[tauri::command]
fn set_zoom(
    app: AppHandle,
    zoom: f64,
    engine: tauri::State<'_, engine::StateEngine>,
) -> Result<(), String> {
    if !engine.apply_zoom(zoom) {
        // 与当前值相同：无需写盘/重排（界面档位未变化）
        return Ok(());
    }
    prefs::save_prefs(&app, &engine.prefs())?;
    resize_persona_window(&app);
    let (w, h) = engine.display_size();
    let _ = app.emit("zoom-changed", engine::ZoomChanged { w, h });
    Ok(())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// 设置 AI 每日气泡开关：写 prefs 并落盘；开启时视条件立即补跑当日生成。
#[tauri::command]
fn set_ai_bubbles(
    app: AppHandle,
    enabled: bool,
    engine: tauri::State<'_, engine::StateEngine>,
) -> Result<(), String> {
    engine.set_ai_bubbles(enabled);
    prefs::save_prefs(&app, &engine.prefs())?;
    if enabled {
        genbubble::maybe_spawn_for_state(&app);
    }
    Ok(())
}

/// 前端在事件挂载完成后调用：把启动时的角色/状态广播给各窗口，并排出第一条环境气泡。
/// setup 阶段 WebView 尚未注册监听，提前 broadcast 会被丢弃，因此推迟到前端就绪。
#[tauri::command]
fn frontend_ready(
    engine: tauri::State<'_, engine::StateEngine>,
    state: tauri::State<'_, AppState>,
) {
    // 用原子标志保证只广播一次：前端重载/重复调用时不重复弹开场气泡。
    if state.frontend_ready.swap(true, Ordering::SeqCst) {
        return;
    }
    engine.broadcast_state();
}

/// 对话入口：组装 persona/state system prompt + 历史消息，调用大模型
#[tauri::command]
async fn chat_send(
    app: AppHandle,
    messages: Vec<llm::LlmMessage>,
    engine: tauri::State<'_, engine::StateEngine>,
    clipboard_image: Option<String>,
) -> Result<ChatReply, String> {
    // 记录聊天互动：随后数分钟内抑制环境气泡（即使本轮回复失败，用户也在互动中）
    engine.record_chat_activity();
    let state = engine.current_state();
    let cfg = llm::load_config(&app);
    let persona = engine.persona();
    // 注入当前正在播放的动作，避免模型回答“在磨剑”这类画面里根本不存在的动作
    let activity = engine.current_activity();
    let mut system = engine::build_system_prompt(&persona, &state, activity.as_ref());

    let mut llm_messages: Vec<llm::LlmMessage> = messages
        .into_iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .collect();

    if let Some(img) = clipboard_image {
        if !img.is_empty() {
            llm::attach_image_to_last_user(&mut llm_messages, img);
            system.push_str(
                "\n\n【本次回答】用户给你看了一张图片（屏幕截图或粘贴的图片）。\
                 请以图片内容作为依据回答；若问题与图片无关或看不清，请如实说明。",
            );
        }
    }
    llm_messages.insert(
        0,
        llm::LlmMessage {
            role: "system".into(),
            content: system.into(),
        },
    );

    let model = &cfg.model;
    let mut reply = llm::chat_completion_stream(&cfg, model, &llm_messages, |delta| {
        let _ = app.emit("chat-delta", delta);
    })
    .await?;
    // 模型偶发返回空内容时重试一次
    if reply.trim().is_empty() {
        // 重试前通知前端重置输入区（回到“正在输入…”），避免上一轮残留的增量污染新流。
        let _ = app.emit("chat-reset", ());
        reply = llm::chat_completion_stream(&cfg, model, &llm_messages, |delta| {
            let _ = app.emit("chat-delta", delta);
        })
        .await?;
        if reply.trim().is_empty() {
            // 第二次仍为空：不再把空字符串当正常回复返回（否则前端会把一条空 assistant 消息
            // 写进 history，污染后续上下文）。改为报错走 catch：清掉打字、显示错误、撤回用户消息。
            return Err("模型连续两次返回空回复，请检查模型或稍后重试".into());
        }
    }
    Ok(ChatReply { reply })
}

/// 点击「📷」：截取当前屏幕并返回 base64 data URL，供前端作为预览、待用户发送。
/// 抓屏含同步 sleep 与编码，放到阻塞线程池执行，避免卡住 async 运行时。
#[tauri::command]
async fn capture_screen(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || screen::capture_current_monitor_data_url(&app))
        .await
        .map_err(|e| format!("截图任务执行失败：{e}"))?
}

fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let toggle_persona = MenuItem::with_id(app, "toggle_persona", "隐藏角色", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    // 角色子菜单：从引擎注册表动态生成
    let engine = app.state::<engine::StateEngine>();
    let mut persona_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
    let mut persona_item_map = HashMap::new();
    for (id, name) in engine.list_personas() {
        let item = MenuItem::with_id(app, format!("persona_{id}"), name, true, None::<&str>)?;
        persona_item_map.insert(id, item.clone());
        persona_items.push(item);
    }
    let state = app.state::<AppState>();
    crate::util::lock(&state.persona_items).extend(persona_item_map);
    let persona_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = persona_items
        .iter()
        .map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>)
        .collect();
    let persona_menu = Submenu::with_items(app, "更换角色", true, &persona_refs)?;
    crate::util::lock(&state.persona_submenu).replace(persona_menu.clone());

    let menu = Menu::with_items(app, &[&settings, &toggle_persona, &persona_menu, &quit])?;
    crate::util::lock(&state.persona_menu_item).replace(toggle_persona.clone());

    TrayIconBuilder::with_id("deskzen-tray")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("DeskZen 桌面众生")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "settings" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = open_settings(handle).await;
                });
            }
            "toggle_persona" => {
                let visible = app
                    .get_webview_window("persona")
                    .map(|w| w.is_visible().unwrap_or(false))
                    .unwrap_or(false);
                if let Some(win) = app.get_webview_window("persona") {
                    if visible {
                        let _ = win.hide();
                    } else {
                        let _ = win.show();
                    }
                }
                update_persona_menu_label(app);
            }
            id if id.starts_with("persona_") => {
                let persona_id = id.trim_start_matches("persona_").to_string();
                let engine = app.state::<engine::StateEngine>();
                if let Err(e) = engine.switch_persona(app, &persona_id) {
                    let _ = app.emit("bubble", engine::BubbleEvent { text: e });
                }
                update_persona_menu_labels(app);
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button,
                button_state,
                ..
            } = event
            {
                if button_state != MouseButtonState::Up {
                    return;
                }
                let app = tray.app_handle().clone();
                // 左键单击：角色隐藏时把它显示出来
                if button == MouseButton::Left {
                    let visible = app
                        .get_webview_window("persona")
                        .map(|w| w.is_visible().unwrap_or(false))
                        .unwrap_or(false);
                    if !visible {
                        if let Some(win) = app.get_webview_window("persona") {
                            let _ = win.show();
                        }
                        update_persona_menu_label(&app);
                    }
                }
            }
            if let TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } = event
            {
                // 左键双击：打开设置界面
                let app = tray.app_handle().clone();
                tauri::async_runtime::spawn(async move {
                    let _ = open_settings(app).await;
                });
            }
        })
        .build(app)?;
    update_persona_menu_labels(app);
    Ok(())
}

/// 根据角色当前可见性刷新托盘菜单文案
fn update_persona_menu_label(app: &AppHandle) {
    let visible = app
        .get_webview_window("persona")
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    let label = if visible {
        "隐藏角色"
    } else {
        "显示角色"
    };
    let state = app.state::<AppState>();
    let guard = crate::util::lock(&state.persona_menu_item);
    if let Some(item) = guard.as_ref() {
        let _ = item.set_text(label);
    }
}

/// 根据当前激活角色刷新“更换角色”子菜单文案（✓ 标记当前角色）
fn update_persona_menu_labels(app: &AppHandle) {
    let engine = app.state::<engine::StateEngine>();
    let active = engine.persona_id();
    let names: HashMap<String, String> = engine.list_personas().into_iter().collect();
    let state = app.state::<AppState>();
    let guard = crate::util::lock(&state.persona_items);
    for (id, item) in guard.iter() {
        let name = names.get(id).cloned().unwrap_or_default();
        let label = if *id == active {
            format!("✓ {name}")
        } else {
            name
        };
        let _ = item.set_text(label);
    }
}

/// 导入新角色后，往托盘“更换角色”子菜单追加菜单项并刷新 ✓ 标记
pub(crate) fn add_persona_menu_item(app: &AppHandle, id: &str, name: &str) -> Result<(), String> {
    let item = MenuItem::with_id(app, format!("persona_{id}"), name, true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let state = app.state::<AppState>();
    let old_item = crate::util::lock(&state.persona_items).insert(id.to_string(), item.clone());
    // 重复导入同一角色时，persona_items 虽然会覆盖旧菜单项，但旧项仍残留在“更换角色”
    // 子菜单里，导致菜单出现两个同名项（删除时也只会移除一个）；这里先移除旧项，
    // 保证 map 与子菜单始终一一对应（参考 remove_persona_menu_item 的做法）。
    if let Some(old_item) = old_item {
        if let Some(submenu) = crate::util::lock(&state.persona_submenu).as_ref() {
            let _ = submenu.remove(&old_item);
        }
    }
    if let Some(submenu) = crate::util::lock(&state.persona_submenu).as_ref() {
        let _ = submenu.append(&item);
    }
    update_persona_menu_labels(app);
    Ok(())
}

/// 删除角色后，从托盘“更换角色”子菜单移除对应菜单项
pub(crate) fn remove_persona_menu_item(app: &AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let item = {
        let mut guard = crate::util::lock(&state.persona_items);
        guard.remove(id)
    };
    if let (Some(item), Some(submenu)) = (item, crate::util::lock(&state.persona_submenu).as_ref())
    {
        let _ = submenu.remove(&item);
    }
    update_persona_menu_labels(app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_api_key_masks_short_and_long() {
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("short"), "****");
        assert_eq!(mask_api_key("sk-1234567890abcdef"), "sk-****cdef");
    }

    #[test]
    fn decide_api_key_reuses_only_exact_mask() {
        let existing = "sk-1234567890abcdef";
        // 回传与现有 Key 打码结果完全一致 → 沿用旧 Key（用户未改动）
        assert_eq!(
            decide_api_key(mask_api_key(existing).to_string(), existing),
            existing.to_string()
        );
        // 回传一个恰好含 * 但并非打码结果的真实 Key → 视为新值保存（不被误判为打码）
        assert_eq!(
            decide_api_key("sk-12*34*567890".to_string(), existing),
            "sk-12*34*567890".to_string()
        );
        // 现有 Key 为空 → 一律把回传值当作新值保存
        assert_eq!(
            decide_api_key("abc****wxyz".to_string(), ""),
            "abc****wxyz".to_string()
        );
    }
}
