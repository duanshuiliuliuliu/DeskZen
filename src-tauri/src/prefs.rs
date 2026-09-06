use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 全局显示缩放范围（与设置页档位一致）：0.5（50%）~ 2.0（200%）。
pub const MIN_ZOOM: f64 = 0.5;
pub const MAX_ZOOM: f64 = 2.0;
const DEFAULT_ZOOM: f64 = 1.0;

pub(crate) fn default_zoom() -> f64 {
    DEFAULT_ZOOM
}

/// AI 每日气泡开关默认值：默认开启（未配置大模型时功能自动失效，无副作用）
pub(crate) fn default_ai_bubbles() -> bool {
    true
}

/// 缩放值归一化：非 finite（NaN/Inf）时回退默认，避免扩散进显示尺寸计算。
fn normalize_zoom(zoom: f64) -> f64 {
    if zoom.is_finite() {
        zoom
    } else {
        default_zoom()
    }
}

/// 应用级偏好：全局缩放 + AI 每日气泡开关；zoom 是乘数，persona.json 的 display_w/h 保持基准语义不变。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    #[serde(default = "default_zoom")]
    pub zoom: f64,
    /// 是否启用大模型每日生成气泡台词
    #[serde(default = "default_ai_bubbles")]
    pub ai_bubbles: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            zoom: default_zoom(),
            ai_bubbles: default_ai_bubbles(),
        }
    }
}

/// 读取 prefs.json。任何失败（文件缺失/解析错误/zoom 非 finite）都返回默认值，不报错——
/// 偏好读取失败不应阻塞启动，也无需向用户提示。
pub fn load_prefs(app: &AppHandle) -> Prefs {
    let Ok(dir) = app.path().app_config_dir() else {
        return Prefs::default();
    };
    let file = dir.join("prefs.json");
    let Ok(content) = std::fs::read_to_string(file) else {
        return Prefs::default();
    };
    let Ok(mut p) = serde_json::from_str::<Prefs>(&content) else {
        return Prefs::default();
    };
    p.zoom = normalize_zoom(p.zoom);
    p
}

/// 保存 prefs.json：先写临时文件再 rename 原子替换，避免写一半崩溃导致文件截断损坏。
pub fn save_prefs(app: &AppHandle, prefs: &Prefs) -> Result<(), String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join("prefs.json.tmp");
    let json = serde_json::to_string_pretty(prefs).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    // Windows 下 fs::rename 用 MoveFileEx + REPLACE_EXISTING，可原子覆盖已存在的目标文件。
    std::fs::rename(&tmp, dir.join("prefs.json")).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_zoom_defaults_to_1() {
        let p: Prefs = serde_json::from_str("{}").unwrap();
        assert_eq!(p.zoom, 1.0);
        assert!(p.ai_bubbles, "缺省应开启 AI 每日气泡");
    }

    #[test]
    fn zoom_round_trips() {
        let p = Prefs { zoom: 1.5, ai_bubbles: true };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Prefs>(&json).unwrap().zoom, 1.5);
    }

    #[test]
    fn non_numeric_zoom_rejected_by_serde() {
        // 非数值 zoom 会被 serde 拒绝；load_prefs 对此类解析失败统一回退默认值。
        assert!(serde_json::from_str::<Prefs>(r#"{"zoom":"oops"}"#).is_err());
    }

    #[test]
    fn non_finite_zoom_falls_back_to_default() {
        assert_eq!(normalize_zoom(f64::NAN), 1.0);
        assert_eq!(normalize_zoom(f64::INFINITY), 1.0);
        assert_eq!(normalize_zoom(2.0), 2.0);
    }
}
