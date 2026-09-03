use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use tauri::{AppHandle, Manager};

/// 隐藏窗口后等待窗口管理器真正把窗口从合成画面里移除的时长。
/// 太短（如 80ms）在部分机器上会抓到“隐藏前旧帧”，导致窗口仍出现在截图里；
/// 200ms 是被验证能稳定拿到干净画面的值。若要更“无感”，只能降低，但会有拍到窗口的风险。
const HIDE_SETTLE_MS: u64 = 200;

/// 捕获“当前屏幕”的截图，编码为 PNG，并以 base64 data URL 返回，
/// 供多模态大模型在聊天中理解屏幕内容。
pub fn capture_current_monitor_data_url(app: &AppHandle) -> Result<String, String> {
    let monitor = target_monitor(app)?;

    // 先隐藏角色窗与对话窗，避免它们挡住要截的屏幕内容；截完再恢复。
    let hidden = hide_app_windows(app);
    // 给窗口管理器一点时间，让窗口真正从屏幕上移除。
    std::thread::sleep(Duration::from_millis(HIDE_SETTLE_MS));
    let captured = monitor.capture_image();
    restore_app_windows(app, &hidden);

    let image = captured.map_err(|e| format!("屏幕截图失败：{e}"))?;

    let mut png: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, image.width(), image.height());
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG 编码失败：{e}"))?;
        writer
            .write_image_data(image.as_raw())
            .map_err(|e| format!("PNG 编码失败：{e}"))?;
    }

    let b64 = STANDARD.encode(&png);
    Ok(format!("data:image/png;base64,{b64}"))
}

/// 隐藏当前可见的角色窗与对话窗，返回实际被隐藏的窗口标签，方便后续恢复。
fn hide_app_windows(app: &AppHandle) -> Vec<&'static str> {
    let mut hidden = Vec::new();
    for label in ["persona", "chat"] {
        if let Some(win) = app.get_webview_window(label) {
            if win.is_visible().unwrap_or(false) {
                let _ = win.hide();
                hidden.push(label);
            }
        }
    }
    hidden
}

/// 按之前记录的窗口恢复显示；对话窗额外取回焦点，保证能继续输入。
fn restore_app_windows(app: &AppHandle, hidden: &[&str]) {
    for &label in hidden {
        if let Some(win) = app.get_webview_window(label) {
            let _ = win.show();
            if label == "chat" {
                let _ = win.set_focus();
            }
        }
    }
}

/// 选择“当前屏幕”：优先取角色 / 对话窗所在的显示器，找不到则回退到主屏。
fn target_monitor(app: &AppHandle) -> Result<xcap::Monitor, String> {
    let point = app
        .get_webview_window("persona")
        .and_then(|w| w.outer_position().ok())
        .or_else(|| {
            app.get_webview_window("chat")
                .and_then(|w| w.outer_position().ok())
        });
    if let Some(pos) = point {
        // 取窗口内侧一点，避免恰好落在显示器边界导致 from_point 常失败
        if let Ok(m) = xcap::Monitor::from_point(pos.x + 1, pos.y + 1) {
            return Ok(m);
        }
    }
    let monitors = xcap::Monitor::all().map_err(|e| format!("枚举显示器失败：{e}"))?;
    monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| xcap::Monitor::all().ok().and_then(|v| v.into_iter().next()))
        .ok_or_else(|| "未找到可用的显示器".into())
}
