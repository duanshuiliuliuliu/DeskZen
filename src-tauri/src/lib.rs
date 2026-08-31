mod engine;
mod llm;

use std::{
    collections::HashMap,
    sync::Mutex,
};

use tauri::{
    menu::{Menu, MenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder,
};

/// 应用级共享状态
pub struct AppState {
    /// 是否处于整窗点击穿透模式
    pub passthrough: Mutex<bool>,
    /// 托盘菜单里的“显示/隐藏角色”项，用于动态更新文案
    pub persona_menu_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    /// 托盘“更换角色”子菜单项，id -> 菜单项
    pub persona_items: Mutex<HashMap<String, MenuItem<tauri::Wry>>>,
}

#[derive(serde::Serialize)]
struct ChatReply {
    reply: String,
    state: String,
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState {
            passthrough: Mutex::new(false),
            persona_menu_item: Mutex::new(None),
            persona_items: Mutex::new(HashMap::new()),
        })
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
                let _ = persona.set_position(PhysicalPosition::new(
                    pos.x + size.width as i32 - 240 - 40,
                    pos.y + size.height as i32 - 280 - 70,
                ));
            }
            persona.show()?;

            // 角色被拖动时，让对话窗跟着移动，保持“面对面聊天”的感觉
            let app_handle = app.handle().clone();
            persona.on_window_event(move |event| {
                if let tauri::WindowEvent::Moved(_) = event {
                    if let Some(chat) = app_handle.get_webview_window("chat") {
                        if chat.is_visible().unwrap_or(false) {
                            reposition_chat(&app_handle);
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
            get_llm_config,
            save_llm_config,
            get_persona_visible,
            set_persona_visible,
            get_passthrough,
            set_passthrough,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("DeskZen 启动失败");
}

/// 点击角色/气泡后打开（或聚焦）对话窗口
#[tauri::command]
async fn open_chat(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("chat") {
        reposition_chat(&app);
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(&app, "chat", WebviewUrl::App("chat.html".into()))
        .title("DeskZen · 对话")
        .inner_size(420.0, 560.0)
        .min_inner_size(320.0, 400.0)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    reposition_chat(&app);
    if let Some(win) = app.get_webview_window("chat") {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

/// 把对话窗摆到角色旁边（默认右侧，右侧放不下则左侧），并限制在显示器工作区内
fn reposition_chat(app: &AppHandle) {
    let Some(persona) = app.get_webview_window("persona") else { return };
    let Some(chat) = app.get_webview_window("chat") else { return };
    let Ok(p_pos) = persona.outer_position() else { return };
    let Ok(p_size) = persona.outer_size() else { return };
    let Ok(c_size) = chat.outer_size() else { return };
    let gap: i32 = 16;
    let mut x = p_pos.x + p_size.width as i32 + gap;
    let mut y = p_pos.y + (p_size.height as i32 - c_size.height as i32) / 2;
    if let Ok(Some(monitor)) = persona.current_monitor() {
        let wa = monitor.work_area();
        let min_x = wa.position.x;
        let min_y = wa.position.y;
        let max_x = min_x + wa.size.width as i32;
        let max_y = min_y + wa.size.height as i32;
        // 右侧放不下就换到左侧
        if x + c_size.width as i32 > max_x {
            x = p_pos.x - c_size.width as i32 - gap;
        }
        x = x.clamp(min_x, max_x - c_size.width as i32);
        y = y.clamp(min_y, max_y - c_size.height as i32);
    }
    let _ = chat.set_position(tauri::PhysicalPosition::new(x, y));
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
        .inner_size(440.0, 620.0)
        .min_inner_size(360.0, 500.0)
        .center()
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

#[derive(serde::Serialize)]
struct LlmConfigView {
    base_url: String,
    model: String,
    api_key: String,
}

#[tauri::command]
fn get_llm_config(app: AppHandle) -> LlmConfigView {
    let cfg = llm::load_config(&app);
    LlmConfigView {
        base_url: cfg.base_url,
        model: cfg.model,
        api_key: cfg.api_key,
    }
}

#[tauri::command]
fn save_llm_config(
    app: AppHandle,
    base_url: String,
    model: String,
    api_key: String,
) -> Result<(), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let cfg = llm::LlmConfig {
        base_url,
        model,
        api_key,
    };
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("llm.json"), json).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_persona_visible(app: AppHandle) -> bool {
    app.get_webview_window("persona")
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false)
}

#[tauri::command]
fn set_persona_visible(app: AppHandle, visible: bool) {
    if let Some(win) = app.get_webview_window("persona") {
        if visible {
            let _ = win.show();
        } else {
            let _ = win.hide();
        }
    }
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

/// 对话入口：组装 persona/state system prompt + 历史消息，调用 DeepSeek
#[tauri::command]
async fn chat_send(
    app: AppHandle,
    messages: Vec<llm::LlmMessage>,
    engine: tauri::State<'_, engine::StateEngine>,
) -> Result<ChatReply, String> {
    let state = engine.current_state();
    let cfg = llm::load_config(&app);
    let persona = engine.persona();
    let system = engine::build_system_prompt(&persona, &state);

    let mut llm_messages = Vec::with_capacity(messages.len() + 1);
    llm_messages.push(llm::LlmMessage {
        role: "system".into(),
        content: system,
    });
    llm_messages.extend(
        messages
            .into_iter()
            .filter(|m| m.role == "user" || m.role == "assistant"),
    );

    let reply = llm::chat_completion(&cfg, &llm_messages).await?;
    let _ = app.emit(
        "bubble",
        engine::BubbleEvent {
            state: state.clone(),
            text: "汪，收到！".into(),
        },
    );
    Ok(ChatReply { reply, state })
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
