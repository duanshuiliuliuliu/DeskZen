use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{
    header::{HeaderMap, HeaderValue, REFERER, USER_AGENT},
    Url,
};
use serde::Deserialize;
use tauri::{AppHandle, Manager};

use crate::engine::{
    LoopEntry, PersonaConfig, ScheduleConfig, StateConfig, StateEngine, SystemPromptConfig,
    TimeSlot,
};

const PETDEX_SITE: &str = "https://petdex.dev";
const PETDEX_REFERER: &str = "https://petdex.dev/";
const PETDEX_USER_AGENT: &str = concat!("DeskZen/", env!("CARGO_PKG_VERSION"));
/// 下载响应与 zip 解压的单个文件大小上限（64 MB）：防超大文件 / zip 炸弹把内容读入内存。
const MAX_PACK_BYTES: usize = 64 * 1024 * 1024;

/// 校验实际大小是否超过上限；超限返回中文错误（含上限值）。
fn ensure_size_within_limit(size: usize, limit: usize) -> Result<(), String> {
    if size > limit {
        Err(format!("文件大小超过上限 {limit} 字节"))
    } else {
        Ok(())
    }
}

#[derive(serde::Serialize)]
pub struct ImportedPet {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct InstallPetResponse {
    ok: bool,
    pet: Option<InstallPet>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct InstallPet {
    slug: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "petJsonUrl")]
    pet_json_url: String,
    #[serde(rename = "spritesheetUrl")]
    spritesheet_url: String,
    #[serde(rename = "spriteExt")]
    sprite_ext: String,
}

/// petdex 包内 pet.json 的元数据（“对角色的简单表述”）
#[derive(Deserialize)]
struct PetJson {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    description: Option<String>,
}

/// 从 petdex 链接导入角色：
/// 1. 校验链接并解析 slug
/// 2. 通过官方程序化接口 /api/install-pet/{slug} 拿到资源 URL
/// 3. 优先下载 zip 并解压出 pet.json + spritesheet；失败则直接下载两个文件
/// 4. 生成 DeskZen persona 配置，写入用户数据目录并注册进状态引擎
#[tauri::command]
pub async fn import_petdex_pet(
    app: AppHandle,
    url: String,
) -> Result<ImportedPet, String> {
    let slug = parse_petdex_slug(&url)?;
    let client = http_client();

    let pet = fetch_install_info(&client, &slug).await?;
    let sprite_ext = if pet.sprite_ext.eq_ignore_ascii_case("png") {
        "png"
    } else {
        "webp"
    };

    // 优先按用户描述的 zip 包下载；zip 不可用时回退到两个文件直连
    let mut pack = match download_zip(&client, &pet).await {
        Ok(p) => p,
        Err(_) => {
            let pet_json = fetch_bytes(&client, &pet.pet_json_url).await?;
            let sprite = fetch_bytes(&client, &pet.spritesheet_url).await?;
            HashMap::from([("pet.json".into(), pet_json), (format!("spritesheet.{sprite_ext}"), sprite)])
        }
    };

    // 下载保持 async；zip 解压、pet.json 解析、写盘与注册都是阻塞操作，
    // 整体移入 spawn_blocking 避免卡住 async 运行时。引擎在阻塞线程内重新获取。
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let pet_json_bytes = pack
            .remove("pet.json")
            .ok_or_else(|| "包内缺少 pet.json".to_string())?;
        let sprite = pack
            .remove(&format!("spritesheet.{sprite_ext}"))
            .or_else(|| pack.remove("spritesheet.webp"))
            .or_else(|| pack.remove("spritesheet.png"))
            .ok_or_else(|| format!("包内缺少 spritesheet.{sprite_ext}"))?;

        let source: PetJson = serde_json::from_slice(&pet_json_bytes)
            .map_err(|e| format!("pet.json 解析失败: {e}"))?;
        let name = source
            .display_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| pet.display_name.clone());
        let id = format!("petdex-{slug}");

        let engine = app_handle.state::<StateEngine>();
        // 写入用户数据目录：%APPDATA%\com.deskzen.app\characters\{id}\
        let pet_dir = engine.characters_dir()?.join(&id);
        fs::create_dir_all(&pet_dir).map_err(|e| format!("创建角色目录失败: {e}"))?;
        let sprite_name = format!("spritesheet.{sprite_ext}");
        let sprite_path = pet_dir.join(&sprite_name);
        fs::write(&sprite_path, &sprite).map_err(|e| format!("写入精灵图失败: {e}"))?;
        fs::write(pet_dir.join("pet.json"), &pet_json_bytes)
            .map_err(|e| format!("写入 pet.json 失败: {e}"))?;

        let persona = build_persona(&id, &name, source.description.as_deref(), sprite_path);
        let persona_json = serde_json::to_string_pretty(&persona)
            .map_err(|e| format!("生成角色配置失败: {e}"))?;
        fs::write(pet_dir.join("persona.json"), &persona_json)
            .map_err(|e| format!("写入角色配置失败: {e}"))?;

        // 运行时注册 + 立即切换展示 + 托盘“更换角色”菜单追加（追加时会刷新 ✓ 标记）
        engine.register_persona(persona.clone());
        engine.switch_persona(&app_handle, &id)?;
        crate::add_persona_menu_item(&app_handle, &id, &persona.name)?;

        Ok::<ImportedPet, String>(ImportedPet {
            id,
            name: persona.name,
        })
    })
    .await
    .map_err(|e| format!("导入任务执行失败: {e}"))?
}

/// 删除已导入的角色（仅允许 petdex- 前缀的导入角色）
#[tauri::command]
pub fn delete_persona(
    app: AppHandle,
    id: String,
    engine: tauri::State<'_, StateEngine>,
) -> Result<(), String> {
    engine.remove_persona(&id)?;
    crate::remove_persona_menu_item(&app, &id);
    // 删除的是当前角色时，回退到内置默认角色
    if engine.persona_id() == id {
        engine.switch_persona(&app, "link")?;
    }
    Ok(())
}

// ---------- 本地导入（zip / 文件夹） ----------

/// 从本地 zip 或文件夹读取 persona.json 与 spritesheet.webp 的内容
fn read_local_pack(path: &Path) -> Result<(Vec<u8>, Vec<u8>), String> {
    use std::io::Cursor;
    if path.is_dir() {
        let pj = find_file_recursive(path, "persona.json")
            .ok_or_else(|| "文件夹内缺少 persona.json".to_string())?;
        let spr = find_file_recursive(path, "spritesheet.webp")
            .ok_or_else(|| "文件夹内缺少 spritesheet.webp".to_string())?;
        let pj = fs::read(&pj).map_err(|e| format!("读取 persona.json 失败: {e}"))?;
        let spr = fs::read(&spr).map_err(|e| format!("读取 spritesheet 失败: {e}"))?;
        Ok((pj, spr))
    } else if path.is_file() {
        let bytes = fs::read(path).map_err(|e| format!("读取文件失败: {e}"))?;
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("zip 解析失败: {e}"))?;
        let mut pj = None;
        let mut spr = None;
        for i in 0..archive.len() {
            let mut f = archive
                .by_index(i)
                .map_err(|e| format!("zip 读取失败: {e}"))?;
            // 用 central directory 元数据对单文件做上限检查，防止 zip 炸弹把超大声明读入内存
            ensure_size_within_limit(f.size() as usize, MAX_PACK_BYTES)?;
            let base = f
                .name()
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(f.name())
                .to_ascii_lowercase();
            if base == "persona.json" && pj.is_none() {
                let mut buf = Vec::with_capacity(f.size() as usize);
                f.read_to_end(&mut buf)
                    .map_err(|e| format!("读取 persona.json 失败: {e}"))?;
                pj = Some(buf);
            } else if base == "spritesheet.webp" && spr.is_none() {
                let mut buf = Vec::with_capacity(f.size() as usize);
                f.read_to_end(&mut buf)
                    .map_err(|e| format!("读取 spritesheet 失败: {e}"))?;
                spr = Some(buf);
            }
        }
        Ok((
            pj.ok_or_else(|| "zip 内缺少 persona.json".to_string())?,
            spr.ok_or_else(|| "zip 内缺少 spritesheet.webp".to_string())?,
        ))
    } else {
        Err("路径不是文件也不是文件夹".into())
    }
}

/// 递归查找目录下的文件（按文件名匹配，忽略大小写）
fn find_file_recursive(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if let Some(f) = find_file_recursive(&p, name) {
                return Some(f);
            }
        } else if p
            .file_name()
            .map(|f| f.to_string_lossy().eq_ignore_ascii_case(name))
            .unwrap_or(false)
        {
            return Some(p);
        }
    }
    None
}

/// 校验 WebP：RIFF....WEBP
fn validate_webp(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok(())
    } else {
        Err("spritesheet 不是有效的 WebP 图片".into())
    }
}

/// 从 id/name 生成安全的 slug
fn sanitize_slug(id: &str, name: &str) -> String {
    let s = if id.trim().is_empty() { name } else { id };
    let s: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "char".to_string()
    } else if s.len() > 40 {
        s[..40].to_string()
    } else {
        s
    }
}

/// 从本地 zip / 文件夹导入角色：校验 persona.json 格式与 WebP 后写入并注册
#[tauri::command]
pub async fn import_local_character(
    app: AppHandle,
    path: String,
    engine: tauri::State<'_, StateEngine>,
) -> Result<ImportedPet, String> {
    // read_local_pack 会读整个 zip / 递归扫描文件夹，是阻塞操作，移入 spawn_blocking。
    let path_buf = PathBuf::from(&path);
    let read_result = tauri::async_runtime::spawn_blocking(move || read_local_pack(&path_buf))
        .await
        .map_err(|e| format!("导入任务执行失败: {e}"))?;
    let (pj_bytes, spr_bytes) = read_result?;
    let mut persona: PersonaConfig =
        serde_json::from_slice(&pj_bytes).map_err(|e| format!("persona.json 格式错误: {e}"))?;
    validate_webp(&spr_bytes)?;

    let id = format!("local-{}", sanitize_slug(&persona.id, &persona.name));
    persona.id = id.clone();
    if persona.name.trim().is_empty() {
        persona.name = persona.id.clone();
    }

    let dir = engine.characters_dir()?.join(&id);
    fs::create_dir_all(&dir).map_err(|e| format!("创建角色目录失败: {e}"))?;
    let sprite_path = dir.join("spritesheet.webp");
    fs::write(&sprite_path, &spr_bytes).map_err(|e| format!("写入精灵图失败: {e}"))?;
    persona.spritesheet = sprite_path.to_string_lossy().into_owned();
    let persona_json = serde_json::to_string_pretty(&persona)
        .map_err(|e| format!("生成角色配置失败: {e}"))?;
    fs::write(dir.join("persona.json"), &persona_json)
        .map_err(|e| format!("写入角色配置失败: {e}"))?;

    engine.register_persona(persona.clone());
    if let Err(e) = engine.switch_persona(&app, &id) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e);
    }
    crate::add_persona_menu_item(&app, &id, &persona.name)?;
    Ok(ImportedPet {
        id,
        name: persona.name,
    })
}

fn http_client() -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, HeaderValue::from_static(PETDEX_REFERER));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(PETDEX_USER_AGENT),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap_or_default()
}

/// 只接受 https://petdex.dev/pets/{slug} 形式的链接
fn parse_petdex_slug(url: &str) -> Result<String, String> {
    let parsed = Url::parse(url).map_err(|_| "链接格式不正确".to_string())?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("petdex.dev") {
        return Err("仅支持 https://petdex.dev/pets/… 链接".into());
    }
    let mut segs = parsed
        .path_segments()
        .ok_or_else(|| "链接格式不正确".to_string())?;
    if segs.next() != Some("pets") {
        return Err("链接必须以 /pets/ 开头".into());
    }
    let slug = segs.next().ok_or_else(|| "链接缺少角色 slug".to_string())?;
    // 容忍尾部斜杠（如 …/pets/doraemon/）
    match segs.next() {
        None => {}
        Some("") if segs.next().is_none() => {}
        _ => return Err("链接路径过长，只需 /pets/{slug}".into()),
    }
    if slug.is_empty()
        || slug.len() > 63
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err("角色 slug 不合法".into());
    }
    Ok(slug.to_string())
}

async fn fetch_install_info(client: &reqwest::Client, slug: &str) -> Result<InstallPet, String> {
    let url = format!("{PETDEX_SITE}/api/install-pet/{slug}");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("请求 petdex 失败: {e}"))?;
    let status = resp.status();
    let body: InstallPetResponse = resp
        .json()
        .await
        .map_err(|e| format!("解析 petdex 响应失败: {e}"))?;
    if !body.ok {
        return Err(body
            .error
            .unwrap_or_else(|| format!("petdex 返回失败 (HTTP {status})")));
    }
    body.pet
        .ok_or_else(|| format!("未找到角色「{slug}」"))
}

/// 由 petJsonUrl 推导 zip 地址（pets/ 目录固定 zip.zip；community/ 目录为 {slug}.zip）
fn zip_url_for(pet: &InstallPet) -> String {
    let Some((dir, file)) = pet.pet_json_url.rsplit_once('/') else {
        return format!("{}/zip.zip", pet.pet_json_url.trim_end_matches('/'));
    };
    if file.eq_ignore_ascii_case("pet.json") {
        format!("{dir}/{}.zip", pet.slug)
    } else {
        format!("{dir}/zip.zip")
    }
}

async fn download_zip(
    client: &reqwest::Client,
    pet: &InstallPet,
) -> Result<HashMap<String, Vec<u8>>, String> {
    let bytes = fetch_bytes(client, &zip_url_for(pet)).await?;
    extract_pack(&bytes)
}

/// 解压 zip，挑出 pet.json 与 spritesheet（支持文件在子目录里，按文件名取）
fn extract_pack(bytes: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    use std::io::Cursor;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("zip 解析失败: {e}"))?;
    let mut out = HashMap::new();
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip 读取失败: {e}"))?;
        // 用 central directory 元数据对单文件做上限检查，防止 zip 炸弹把超大声明读入内存
        ensure_size_within_limit(file.size() as usize, MAX_PACK_BYTES)?;
        let base = file
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(file.name())
            .to_ascii_lowercase();
        if base == "pet.json" && !out.contains_key("pet.json") {
            let mut buf = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut buf)
                .map_err(|e| format!("读取 pet.json 失败: {e}"))?;
            out.insert("pet.json".into(), buf);
        } else if let Some(ext) = base.strip_prefix("spritesheet.") {
            let key = format!("spritesheet.{ext}");
            if (ext == "webp" || ext == "png") && !out.contains_key(&key)
            {
                let mut buf = Vec::with_capacity(file.size() as usize);
                file.read_to_end(&mut buf)
                    .map_err(|e| format!("读取 spritesheet 失败: {e}"))?;
                out.insert(key, buf);
            }
        }
    }
    if !out.contains_key("pet.json") {
        return Err("zip 内缺少 pet.json".into());
    }
    if !(out.contains_key("spritesheet.webp") || out.contains_key("spritesheet.png")) {
        return Err("zip 内缺少 spritesheet".into());
    }
    Ok(out)
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("下载失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("下载失败 (HTTP {status}): {url}"));
    }
    // 预检 content-length：Some 且超限直接报错，避免把超大响应读入内存
    if let Some(len) = resp.content_length() {
        ensure_size_within_limit(len as usize, MAX_PACK_BYTES)?;
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取下载内容失败: {e}"))?;
    // 下载完成后校验实际字节数（content-length 可能缺失或与实际不符）
    ensure_size_within_limit(bytes.len(), MAX_PACK_BYTES)?;
    Ok(bytes.to_vec())
}

/// 把 petdex 的 pet.json 简单表述 + 固定 8x9 网格约定，映射为 DeskZen persona 配置
fn build_persona(id: &str, name: &str, description: Option<&str>, sprite_path: PathBuf) -> PersonaConfig {
    let definition = description
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("你是一个来自 Petdex 的小角色「{name}」，现在住在用户的桌面上，有自己的生活节律。"));
    PersonaConfig {
        id: id.to_string(),
        name: name.to_string(),
        spritesheet: sprite_path.to_string_lossy().into_owned(),
        cols: 8,
        rows: 9,
        pixel_art: false,
        display_w: 0,
        display_h: 0,
        system_prompt: SystemPromptConfig {
            definition,
            reply_style: "回复简短俏皮、口语化，像桌面上有性格的小角色；不要长篇大论，不要使用列表和 Markdown；永远不要提及自己是 AI 或程序。".into(),
            state_guidelines: default_guidelines(),
        },
        states: default_states(),
        schedule: default_schedule(),
    }
}

fn default_states() -> HashMap<String, StateConfig> {
    let mut states = HashMap::new();
    states.insert(
        "Idle".into(),
        StateConfig {
            label: "待机".into(),
            row: 0,
            frames: 6,
            frame_ms: 180,
            bubbles: vec!["嘿嘿，我在待机中～".into(), "要不要一起玩？".into(), "我超厉害的！".into()],
        },
    );
    states.insert(
        "RunRight".into(),
        StateConfig {
            label: "向右跑".into(),
            row: 1,
            frames: 8,
            frame_ms: 120,
            bubbles: vec!["向右冲！".into(), "跑起来咯！".into()],
        },
    );
    states.insert(
        "RunLeft".into(),
        StateConfig {
            label: "向左跑".into(),
            row: 2,
            frames: 8,
            frame_ms: 120,
            bubbles: vec!["向左冲！".into(), "嗖——".into()],
        },
    );
    states.insert(
        "Waving".into(),
        StateConfig {
            label: "挥手".into(),
            row: 3,
            frames: 4,
            frame_ms: 160,
            bubbles: vec!["嗨！你好呀！".into(), "挥手挥手～".into()],
        },
    );
    states.insert(
        "Jumping".into(),
        StateConfig {
            label: "跳跃".into(),
            row: 4,
            frames: 5,
            frame_ms: 140,
            bubbles: vec!["跳高高！".into(), "嘿嘿，接住！".into()],
        },
    );
    states.insert(
        "Failed".into(),
        StateConfig {
            label: "失败".into(),
            row: 5,
            frames: 8,
            frame_ms: 130,
            bubbles: vec!["哎呀，摔了一跤…".into(), "呜哇，失败了！".into()],
        },
    );
    states.insert(
        "Waiting".into(),
        StateConfig {
            label: "等待".into(),
            row: 6,
            frames: 6,
            frame_ms: 220,
            bubbles: vec!["我在等什么呢…".into(), "（等ing）".into()],
        },
    );
    states.insert(
        "Running".into(),
        StateConfig {
            label: "跑步".into(),
            row: 7,
            frames: 6,
            frame_ms: 130,
            bubbles: vec!["跑跑跑！".into(), "冲刺！".into()],
        },
    );
    states.insert(
        "Review".into(),
        StateConfig {
            label: "复习".into(),
            row: 8,
            frames: 6,
            frame_ms: 200,
            bubbles: vec!["复习功课中…".into(), "这个字怎么写来着？".into()],
        },
    );
    states
}

fn default_guidelines() -> HashMap<String, String> {
    let mut g = HashMap::new();
    g.insert("Idle".into(), "你正在待机，精神不错，愿意聊天。".into());
    g.insert("RunRight".into(), "你正向右跑，边跑边聊，回复简短活泼。".into());
    g.insert("RunLeft".into(), "你正向左跑，边跑边聊，回复简短活泼。".into());
    g.insert("Waving".into(), "你在挥手打招呼，热情简短地回应。".into());
    g.insert("Jumping".into(), "你在跳跃玩耍，兴奋又闹腾。".into());
    g.insert("Failed".into(), "你刚刚失败了、摔了一跤，有点沮丧，只肯简短嘟囔几句。".into());
    g.insert("Waiting".into(), "你在等待，有点无聊，回复简短。".into());
    g.insert("Running".into(), "你在跑步，气喘吁吁，回复很短。".into());
    g.insert("Review".into(), "你在复习功课，认真专注，回复简短。".into());
    g
}

fn default_schedule() -> ScheduleConfig {
    ScheduleConfig {
        r#loop: vec![
            LoopEntry { state: "Idle".into(), duration: 5 },
            LoopEntry { state: "Waving".into(), duration: 5 },
            LoopEntry { state: "Jumping".into(), duration: 5 },
            LoopEntry { state: "RunRight".into(), duration: 5 },
            LoopEntry { state: "RunLeft".into(), duration: 5 },
        ],
        time: vec![
            TimeSlot { start: "00:00".into(), end: "07:00".into(), state: "Waiting".into() },
            TimeSlot { start: "08:30".into(), end: "10:00".into(), state: "Review".into() },
            TimeSlot { start: "12:00".into(), end: "13:30".into(), state: "Waiting".into() },
            TimeSlot { start: "13:30".into(), end: "15:00".into(), state: "Review".into() },
            TimeSlot { start: "18:30".into(), end: "20:00".into(), state: "Review".into() },
            TimeSlot { start: "22:00".into(), end: "24:00".into(), state: "Waiting".into() },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_petdex_slug() {
        assert_eq!(
            parse_petdex_slug("https://petdex.dev/pets/doraemon").unwrap(),
            "doraemon"
        );
        assert_eq!(
            parse_petdex_slug("https://petdex.dev/pets/doraemon/").unwrap(),
            "doraemon"
        );
        assert!(parse_petdex_slug("https://petdex.dev/pets/").is_err());
        assert!(parse_petdex_slug("https://petdex.dev/pets/a/b").is_err());
        assert!(parse_petdex_slug("https://example.com/pets/doraemon").is_err());
        assert!(parse_petdex_slug("https://petdex.dev/foo/doraemon").is_err());
        assert!(parse_petdex_slug("https://petdex.dev/pets/../evil").is_err());
    }

    #[test]
    fn derives_zip_url() {
        let pet = InstallPet {
            slug: "doraemon".into(),
            display_name: "Doraemon".into(),
            pet_json_url: "https://assets.petdex.dev/pets/doraemon-58b12a5012e0/petjson.json".into(),
            spritesheet_url: "https://assets.petdex.dev/pets/doraemon-58b12a5012e0/sprite.webp".into(),
            sprite_ext: "webp".into(),
        };
        assert_eq!(
            zip_url_for(&pet),
            "https://assets.petdex.dev/pets/doraemon-58b12a5012e0/zip.zip"
        );

        let community = InstallPet {
            slug: "kaka".into(),
            display_name: "Kaka".into(),
            pet_json_url: "https://assets.petdex.dev/community/kaka/pet.json".into(),
            spritesheet_url: "https://assets.petdex.dev/community/kaka/spritesheet.webp".into(),
            sprite_ext: "webp".into(),
        };
        assert_eq!(
            zip_url_for(&community),
            "https://assets.petdex.dev/community/kaka/kaka.zip"
        );
    }

    #[test]
    fn extracts_pack_from_zip() {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer.start_file("pet.json", opts).unwrap();
            writer.write_all(br#"{"displayName":"Doraemon"}"#).unwrap();
            writer.start_file("spritesheet.webp", opts).unwrap();
            writer.write_all(b"webp-data").unwrap();
            writer.finish().unwrap();
        }
        let pack = extract_pack(&buf).unwrap();
        assert_eq!(pack.get("pet.json").unwrap(), br#"{"displayName":"Doraemon"}"#);
        assert_eq!(pack.get("spritesheet.webp").unwrap(), b"webp-data");
    }

    #[test]
    fn size_limit_rejects_oversize() {
        // 未超限（含恰好等于上限）允许通过
        assert!(ensure_size_within_limit(MAX_PACK_BYTES, MAX_PACK_BYTES).is_ok());
        assert!(ensure_size_within_limit(10, MAX_PACK_BYTES).is_ok());
        // 超限时报错，且错误信息包含上限值
        let err = ensure_size_within_limit(MAX_PACK_BYTES + 1, MAX_PACK_BYTES).unwrap_err();
        assert!(
            err.contains(&MAX_PACK_BYTES.to_string()),
            "错误信息应包含上限值: {err}"
        );
    }

    #[test]
    fn builds_persona_from_pet_meta() {
        let cfg = build_persona(
            "petdex-doraemon",
            "Doraemon",
            Some("A blue robot-cat."),
            PathBuf::from("C:/characters/petdex-doraemon/spritesheet.webp"),
        );
        assert_eq!(cfg.id, "petdex-doraemon");
        assert_eq!(cfg.name, "Doraemon");
        assert_eq!(cfg.cols, 8);
        assert_eq!(cfg.rows, 9);
        assert_eq!(cfg.states.len(), 9);
        assert_eq!(cfg.schedule.time().len(), 6);
        assert!(!cfg.schedule.loop_entries().is_empty());
        assert!(cfg.system_prompt.definition.contains("robot-cat"));
    }
}
