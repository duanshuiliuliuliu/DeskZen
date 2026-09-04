use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use tauri::{AppHandle, Manager};

/// 隐藏窗口后等待窗口管理器真正把窗口从合成画面里移除的时长。
/// 太短（如 80ms）在部分机器上会抓到“隐藏前旧帧”，导致窗口仍出现在截图里；
/// 200ms 是被验证能稳定拿到干净画面的值。若要更“无感”，只能降低，但会有拍到窗口的风险。
const HIDE_SETTLE_MS: u64 = 200;

/// 预览图最长边：超过则按比例缩小，保证 IPC 传回的字符串不会过大。
const PREVIEW_MAX_DIM: u32 = 1280;
/// 预览 JPEG 质量（0~100），与前端 `downscaleToJpeg` 的 0.85 保持一致。
const PREVIEW_JPEG_QUALITY: u8 = 85;

/// 把截图缩放为最长边 ≤1280 的 JPEG 并编码为 base64 data URL。
/// 若原图最长边已 ≤1280，则仅编码 JPEG，不做缩放（幂等，供前端预览直接使用）。
fn encode_preview_data_url(image: &image::RgbaImage) -> Result<String, String> {
    let (w, h) = (image.width(), image.height());
    let max_side = w.max(h);
    let (tw, th) = if max_side > PREVIEW_MAX_DIM {
        let scale = PREVIEW_MAX_DIM as f32 / max_side as f32;
        let tw = ((w as f32 * scale).round() as u32).max(1);
        let th = ((h as f32 * scale).round() as u32).max(1);
        (tw, th)
    } else {
        (w, h)
    };
    let resized = if (tw, th) == (w, h) {
        image.clone()
    } else {
        image::imageops::resize(image, tw, th, image::imageops::FilterType::Triangle)
    };
    let mut jpg: Vec<u8> = Vec::new();
    let dyn_image = image::DynamicImage::ImageRgba8(resized);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, PREVIEW_JPEG_QUALITY)
        .encode_image(&dyn_image)
        .map_err(|e| format!("JPEG 编码失败：{e}"))?;
    let b64 = STANDARD.encode(&jpg);
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

/// 捕获“当前屏幕”的截图，缩放到最长边 ≤1280 后编码为 JPEG，并以 base64 data URL 返回，
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
    // xcap 的 capture_image 返回的本身就是 RgbaImage，直接复用，避免 4K 全屏像素再次拷贝。
    encode_preview_data_url(&image)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 2560x1440 渐变色图，验证最长边超过 1280 时被缩到 1280x720 并编码为 JPEG。
    #[test]
    fn encode_preview_downscales_to_1280() {
        let image = image::RgbaImage::from_fn(2560, 1440, |x, y| {
            image::Rgba([
                (x % 256) as u8,
                (y % 256) as u8,
                ((x + y) % 256) as u8,
                255,
            ])
        });
        let data_url = encode_preview_data_url(&image).expect("编码失败");
        assert!(
            data_url.starts_with("data:image/jpeg;base64,"),
            "data URL 格式错误: {data_url}"
        );
        let b64 = data_url
            .split(',')
            .nth(1)
            .expect("缺少 base64 部分");
        let jpg = STANDARD.decode(b64).expect("base64 解码失败");
        let decoded = image::load_from_memory(&jpg).expect("JPEG 解码失败");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (1280, 720),
            "缩放后尺寸不对"
        );
    }
}
