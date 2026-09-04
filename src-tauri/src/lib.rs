mod engine;
mod llm;
mod petdex;
mod screen;

use std::{
    collections::HashMap,
    sync::Mutex,
};

use tauri::{
    menu::{Menu, MenuEvent, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder,
};

/// 角色精灵在角色窗口内的底距（与 styles.css `.character { bottom: 24px }` 同步）
const SPRITE_BOTTOM: i32 = 24;

/// 应用级共享状态
pub struct AppState {
    /// 是否处于整窗点击穿透模式
    pub passthrough: Mutex<bool>,
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
    state: String,
}

#[derive(serde::Serialize)]
struct PersonaInfo {
    id: String,
    name: String,
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            passthrough: Mutex::new(false),
            persona_menu_item: Mutex::new(None),
            persona_items: Mutex::new(HashMap::new()),
            persona_submenu: Mutex::new(None),
        })
        .on_menu_event(handle_menu_event)
        .setup(|app| {
            let engine = engine::StateEngine::new(app.handle().clone());
            engine.broadcast_state();
            app.manage(engine);
            app.state::<engine::StateEngine>().start();

            setup_tray(app.handle())?;

            // 等前端就绪后再显示，避免透明窗口启动白屏闪烁
            let persona = app.get_webview_window("persona").unwrap();
            // 默认停在主显示器右下角（给任务栏留出空间），避免遮挡居中的对话窗口
            if let Some(monitor) = app.primary_monitor()? {
                let pos = monitor.position();
                let size = monitor.size();
                if let Ok(p_size) = persona.outer_size() {
                    let _ = persona.set_position(PhysicalPosition::new(
                        pos.x + size.width as i32 - p_size.width as i32 - 40,
                        pos.y + size.height as i32 - p_size.height as i32 - 70,
                    ));
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
            chat_send,
            open_chat,
            open_settings,
            switch_persona,
            list_personas,
            petdex::import_petdex_pet,
            petdex::import_local_character,
            petdex::delete_persona,
            get_llm_config,
            save_llm_config,
            get_passthrough,
            set_passthrough,
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
    let persona = app
        .get_webview_window("persona")
        .ok_or("找不到角色窗口")?;
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
    let Some(persona) = app.get_webview_window("persona") else { return };
    let Some(chat) = app.get_webview_window("chat") else { return };
    let Ok(p_pos) = persona.outer_position() else { return };
    let Ok(p_size) = persona.outer_size() else { return };
    let Ok(c_size) = chat.outer_size() else { return };
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
        .inner_size(560.0, 620.0)
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
    engine.switch_persona(&app, &id)
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
    // 前端回传的是打码值（含 *，用户未重新输入）→ 沿用磁盘上已保存的 Key
    let api_key = if api_key.contains('*') {
        llm::load_config(&app).api_key
    } else {
        api_key
    };
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
    *state.passthrough.lock().unwrap()
}

#[tauri::command]
fn set_passthrough(
    app: AppHandle,
    enabled: bool,
    state: tauri::State<'_, AppState>,
) {
    *state.passthrough.lock().unwrap() = enabled;
    if let Some(win) = app.get_webview_window("persona") {
        let _ = win.set_ignore_cursor_events(enabled);
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// 对话入口：组装 persona/state system prompt + 历史消息，调用大模型
#[tauri::command]
async fn chat_send(
    app: AppHandle,
    messages: Vec<llm::LlmMessage>,
    engine: tauri::State<'_, engine::StateEngine>,
    clipboard_image: Option<String>,
) -> Result<ChatReply, String> {
    let state = engine.current_state();
    let cfg = llm::load_config(&app);
    let persona = engine.persona();
    let mut system = engine::build_system_prompt(&persona, &state);

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
    }
    let _ = app.emit(
        "bubble",
        engine::BubbleEvent {
            state: state.clone(),
            text: "收到！".into(),
        },
    );
    Ok(ChatReply { reply, state })
}

/// 点击「📷」：截取当前屏幕并返回 base64 data URL，供前端作为预览、待用户发送。
/// 抓屏含同步 sleep 与编码，放到阻塞线程池执行，避免卡住 async 运行时。
#[tauri::command]
async fn capture_screen(app: AppHandle) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        screen::capture_current_monitor_data_url(&app)
    })
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
    app.state::<AppState>()
        .persona_items
        .lock()
        .unwrap()
        .extend(persona_item_map);
    let persona_refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = persona_items
        .iter()
        .map(|i| i as &dyn tauri::menu::IsMenuItem<tauri::Wry>)
        .collect();
    let persona_menu = Submenu::with_items(app, "更换角色", true, &persona_refs)?;
    app.state::<AppState>()
        .persona_submenu
        .lock()
        .unwrap()
        .replace(persona_menu.clone());

    let menu = Menu::with_items(
        app,
        &[&settings, &toggle_persona, &persona_menu, &quit],
    )?;
    app.state::<AppState>()
        .persona_menu_item
        .lock()
        .unwrap()
        .replace(toggle_persona.clone());

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
                    let _ = app.emit(
                        "bubble",
                        engine::BubbleEvent {
                            state: "Awake".into(),
                            text: e,
                        },
                    );
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
                match button {
                    // 左键单击：角色隐藏时把它显示出来
                    MouseButton::Left => {
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
                    _ => {}
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
    let label = if visible { "隐藏角色" } else { "显示角色" };
    let state = app.state::<AppState>();
    let guard = state.persona_menu_item.lock().unwrap();
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
    let guard = state.persona_items.lock().unwrap();
    for (id, item) in guard.iter() {
        let name = names.get(id).cloned().unwrap_or_default();
        let label = if *id == active { format!("✓ {name}") } else { name };
        let _ = item.set_text(label);
    }
}

/// 导入新角色后，往托盘“更换角色”子菜单追加菜单项并刷新 ✓ 标记
pub(crate) fn add_persona_menu_item(
    app: &AppHandle,
    id: &str,
    name: &str,
) -> Result<(), String> {
    let item = MenuItem::with_id(app, format!("persona_{id}"), name, true, None::<&str>)
        .map_err(|e| e.to_string())?;
    let state = app.state::<AppState>();
    state
        .persona_items
        .lock()
        .unwrap()
        .insert(id.to_string(), item.clone());
    if let Some(submenu) = state.persona_submenu.lock().unwrap().as_ref() {
        let _ = submenu.append(&item);
    }
    update_persona_menu_labels(app);
    Ok(())
}

/// 删除角色后，从托盘“更换角色”子菜单移除对应菜单项
pub(crate) fn remove_persona_menu_item(app: &AppHandle, id: &str) {
    let state = app.state::<AppState>();
    let item = {
        let mut guard = state.persona_items.lock().unwrap();
        guard.remove(id)
    };
    if let (Some(item), Some(submenu)) = (
        item,
        state.persona_submenu.lock().unwrap().as_ref(),
    ) {
        let _ = submenu.remove(&item);
    }
    update_persona_menu_labels(app);
}
