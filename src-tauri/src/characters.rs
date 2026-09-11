use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use tauri::AppHandle;

use crate::engine::{PersonaConfig, StateEngine};

const MAX_PACK_FILE_BYTES: usize = 64 * 1024 * 1024;

#[derive(serde::Serialize)]
pub struct ImportedCharacter {
    pub id: String,
    pub name: String,
}

#[derive(Debug)]
struct CharacterPack {
    persona_json: Vec<u8>,
    files: HashMap<PathBuf, Vec<u8>>,
}

#[tauri::command]
pub async fn import_local_character(
    app: AppHandle,
    path: String,
    engine: tauri::State<'_, StateEngine>,
) -> Result<ImportedCharacter, String> {
    let source = PathBuf::from(path);
    let pack = tauri::async_runtime::spawn_blocking(move || read_character_pack(&source))
        .await
        .map_err(|e| format!("导入任务执行失败: {e}"))??;

    let mut persona: PersonaConfig =
        serde_json::from_slice(&pack.persona_json).map_err(|e| format!("persona.json 格式错误: {e}"))?;
    validate_persona(&persona, &pack.files)?;

    let id = format!("local-{}", sanitize_slug(&persona.id, &persona.name));
    persona.id = id.clone();
    if persona.name.trim().is_empty() {
        persona.name = id.clone();
    }

    let destination = engine.characters_dir()?.join(&id);
    let saved_persona = install_character_pack(&destination, &mut persona, &pack.files)?;
    engine.register_persona(saved_persona.clone());
    if let Err(error) = engine.switch_persona(&app, &id) {
        let _ = fs::remove_dir_all(&destination);
        return Err(error);
    }
    crate::add_persona_menu_item(&app, &id, &saved_persona.name)?;

    Ok(ImportedCharacter {
        id,
        name: saved_persona.name,
    })
}

#[tauri::command]
pub fn delete_persona(
    app: AppHandle,
    id: String,
    engine: tauri::State<'_, StateEngine>,
) -> Result<(), String> {
    if !id.starts_with("local-") {
        return Err("内置角色不可删除".to_string());
    }
    let was_current = engine.persona_id() == id;
    engine.remove_persona(&id)?;
    crate::remove_persona_menu_item(&app, &id);
    if was_current {
        engine.switch_persona(&app, "link")?;
    }
    Ok(())
}

fn read_character_pack(path: &Path) -> Result<CharacterPack, String> {
    if path.is_dir() {
        read_directory_pack(path)
    } else if path.is_file() {
        read_zip_pack(path)
    } else {
        Err("路径不是文件夹或 zip 文件".to_string())
    }
}

fn read_directory_pack(root: &Path) -> Result<CharacterPack, String> {
    let persona_path = root.join("persona.json");
    if !persona_path.is_file() {
        return Err("角色文件夹根目录缺少 persona.json".to_string());
    }
    let persona_json = read_limited(&persona_path)?;
    let clips_root = root.join("clips");
    if !clips_root.is_dir() {
        return Err("角色文件夹根目录缺少 clips 文件夹".to_string());
    }
    let mut files = HashMap::new();
    // 只读 clips/ 下的 WebP：角色包里可能还放着源视频、草稿图等，没必要读进内存
    collect_clip_files(root, &clips_root, &mut files)?;
    Ok(CharacterPack { persona_json, files })
}

fn collect_clip_files(
    root: &Path,
    directory: &Path,
    files: &mut HashMap<PathBuf, Vec<u8>>,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(|e| format!("读取角色目录失败: {e}"))? {
        let entry = entry.map_err(|e| format!("读取角色目录项失败: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_clip_files(root, &path, files)?;
        } else if path.is_file() && has_webp_extension(&path) {
            let relative = path
                .strip_prefix(root)
                .map_err(|e| format!("读取角色资源路径失败: {e}"))?
                .to_path_buf();
            files.insert(relative, read_limited(&path)?);
        }
    }
    Ok(())
}

/// 是否是动作资源后缀（.webp）；不看文件是否存在，zip 里的相对路径也能判断
fn has_webp_extension(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("webp")
}

fn read_zip_pack(path: &Path) -> Result<CharacterPack, String> {
    let bytes = read_limited(path)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("zip 解析失败: {e}"))?;
    let mut files = HashMap::new();
    let mut persona_json = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("zip 读取失败: {e}"))?;
        if entry.is_dir() {
            continue;
        }
        if entry.size() as usize > MAX_PACK_FILE_BYTES {
            return Err("角色包中包含超过 64 MB 的文件".to_string());
        }
        let relative = normalize_relative_path(Path::new(entry.name()))?;
        let is_persona = relative == Path::new("persona.json");
        // 其余条目只关心 clips/ 下的 WebP：其它内容不解压、不占内存
        if !is_persona && !(relative.starts_with("clips") && has_webp_extension(&relative)) {
            continue;
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut content)
            .map_err(|e| format!("读取 zip 内容失败: {e}"))?;
        if is_persona {
            if persona_json.replace(content).is_some() {
                return Err("zip 中包含多个 persona.json".to_string());
            }
        } else {
            files.insert(relative, content);
        }
    }
    Ok(CharacterPack {
        persona_json: persona_json.ok_or_else(|| "zip 根目录缺少 persona.json".to_string())?,
        files,
    })
}

fn validate_persona(persona: &PersonaConfig, files: &HashMap<PathBuf, Vec<u8>>) -> Result<(), String> {
    // 结构判据与启动加载共用（engine::validate_persona_structure），避免两边口径不一致
    crate::engine::validate_persona_structure(persona)?;
    for (clip_id, clip) in &persona.clips {
        let path = normalize_clip_asset(&clip.spritesheet)?;
        let bytes = files
            .get(&path)
            .ok_or_else(|| format!("动作 {clip_id} 缺少资源 {}", path.display()))?;
        validate_webp(bytes).map_err(|e| format!("动作 {clip_id} {e}"))?;
    }
    for (state, scenes) in &persona.scenes {
        if !persona.states.contains_key(state) {
            return Err(format!("场景引用了未定义状态 {state}"));
        }
        if scenes.is_empty() {
            return Err(format!("状态 {state} 没有可播放场景"));
        }
        for scene in scenes {
            if scene.steps.is_empty() {
                return Err(format!("场景 {} 没有动作步骤", scene.id));
            }
            for step in &scene.steps {
                if !persona.clips.contains_key(&step.clip) {
                    return Err(format!("场景 {} 引用了未知动作 {}", scene.id, step.clip));
                }
            }
        }
    }
    Ok(())
}

fn install_character_pack(
    destination: &Path,
    persona: &mut PersonaConfig,
    files: &HashMap<PathBuf, Vec<u8>>,
) -> Result<PersonaConfig, String> {
    let staging = destination.with_extension("installing");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("创建角色目录失败: {e}"))?;
    let result = (|| {
        for clip in persona.clips.values_mut() {
            let relative = normalize_clip_asset(&clip.spritesheet)?;
            let bytes = files
                .get(&relative)
                .ok_or_else(|| format!("缺少动作资源 {}", relative.display()))?;
            let target = staging.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("创建 clips 目录失败: {e}"))?;
            }
            fs::write(&target, bytes).map_err(|e| format!("写入动作资源失败: {e}"))?;
            // 记录最终目录下的路径：staging 随后整体 rename 为 destination，
            // 若这里写入 staging 路径，重命名后配置就会指向不存在的目录。
            clip.spritesheet = destination.join(&relative).to_string_lossy().into_owned();
        }
        let text = serde_json::to_string_pretty(persona)
            .map_err(|e| format!("生成角色配置失败: {e}"))?;
        fs::write(staging.join("persona.json"), text)
            .map_err(|e| format!("写入 persona.json 失败: {e}"))?;
        let _ = fs::remove_dir_all(destination);
        fs::rename(&staging, destination).map_err(|e| format!("完成角色导入失败: {e}"))?;
        Ok(persona.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn normalize_clip_asset(value: &str) -> Result<PathBuf, String> {
    let path = normalize_relative_path(Path::new(value))?;
    if !path.starts_with("clips") {
        return Err("动作资源路径必须位于 clips/ 文件夹内".to_string());
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some("webp") {
        return Err("动作资源必须为 .webp 文件".to_string());
    }
    Ok(path)
}

fn normalize_relative_path(path: &Path) -> Result<PathBuf, String> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => output.push(part),
            _ => return Err("角色包资源路径不安全".to_string()),
        }
    }
    if output.as_os_str().is_empty() {
        Err("角色包资源路径为空".to_string())
    } else {
        Ok(output)
    }
}

fn read_limited(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("读取文件信息失败: {e}"))?;
    if metadata.len() as usize > MAX_PACK_FILE_BYTES {
        return Err("文件大小超过 64 MB".to_string());
    }
    fs::read(path).map_err(|e| format!("读取文件失败: {e}"))
}

fn validate_webp(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok(())
    } else {
        Err("不是有效的 WebP 图片".to_string())
    }
}

fn sanitize_slug(id: &str, name: &str) -> String {
    let source = if id.trim().is_empty() { name } else { id };
    let slug: String = source
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .map(|character| character.to_ascii_lowercase())
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "character".to_string()
    } else {
        slug.chars().take(40).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 最小合法角色包：1 个状态 + 1 个动作 + 1 个场景
    fn minimal_persona_json() -> &'static str {
        r#"{
            "id": "demo",
            "name": "Demo",
            "display_w": 96,
            "display_h": 104,
            "system_prompt": {
                "definition": "演示角色",
                "reply_style": "简短回复"
            },
            "states": {
                "idle": { "label": "待机" }
            },
            "clips": {
                "wave": { "spritesheet": "clips/wave.webp", "frames": 2, "frame_ms": 83 }
            },
            "scenes": {
                "idle": [ { "id": "wave_once", "steps": [ { "clip": "wave" } ] } ]
            },
            "schedule": { "loop": [ { "state": "idle", "duration": 10 } ], "time": [] }
        }"#
    }

    /// RIFF/WEBP 文件头即通过校验（只验证格式魔数，不解码内容）
    fn webp_bytes() -> Vec<u8> {
        b"RIFF\x00\x00\x00\x00WEBP".to_vec()
    }

    fn demo_files() -> HashMap<PathBuf, Vec<u8>> {
        HashMap::from([(PathBuf::from("clips/wave.webp"), webp_bytes())])
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "deskzen-characters-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn validate_persona_requires_clips_scenes_and_per_state_scenes() {
        let mut persona: PersonaConfig = serde_json::from_str(minimal_persona_json()).unwrap();
        assert!(validate_persona(&persona, &demo_files()).is_ok());

        persona.scenes.clear();
        let error = validate_persona(&persona, &demo_files()).unwrap_err();
        assert!(error.contains("scenes"), "{error}");

        persona = serde_json::from_str(minimal_persona_json()).unwrap();
        persona.states.insert(
            "extra".into(),
            serde_json::from_str(r#"{ "label": "额外" }"#).unwrap(),
        );
        let error = validate_persona(&persona, &demo_files()).unwrap_err();
        assert!(error.contains("extra"), "{error}");
    }

    #[test]
    fn validate_persona_rejects_missing_or_invalid_clip_assets() {
        let persona: PersonaConfig = serde_json::from_str(minimal_persona_json()).unwrap();
        let error = validate_persona(&persona, &HashMap::new()).unwrap_err();
        assert!(error.contains("wave"), "{error}");

        let files = HashMap::from([(PathBuf::from("clips/wave.webp"), b"not-webp".to_vec())]);
        let error = validate_persona(&persona, &files).unwrap_err();
        assert!(error.contains("WebP"), "{error}");
    }

    #[test]
    fn install_character_pack_points_assets_at_final_destination() {
        let mut persona: PersonaConfig = serde_json::from_str(minimal_persona_json()).unwrap();
        let root = temp_dir("install");
        let destination = root.join("local-demo");

        let saved = install_character_pack(&destination, &mut persona, &demo_files()).unwrap();
        let expected = destination.join("clips/wave.webp");
        let recorded = PathBuf::from(&saved.clips["wave"].spritesheet);
        assert_eq!(recorded, expected);
        assert!(!recorded.to_string_lossy().contains("installing"));
        assert!(expected.is_file(), "动作资源应落在最终目录");
        assert!(destination.join("persona.json").is_file());
        // staging 目录应已改名，不残留半成品
        assert!(!destination.with_extension("installing").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_pack_reads_only_clip_assets() {
        let root = temp_dir("dir-pack");
        fs::create_dir_all(root.join("clips")).unwrap();
        fs::write(root.join("persona.json"), minimal_persona_json()).unwrap();
        fs::write(root.join("clips/wave.webp"), webp_bytes()).unwrap();
        // 包里常见的"额外内容"：源视频、草稿文件——都不该被读进内存
        fs::write(root.join("source.mp4"), vec![0u8; 4096]).unwrap();
        fs::write(root.join("clips/notes.txt"), b"draft").unwrap();

        let pack = read_directory_pack(&root).unwrap();
        assert_eq!(pack.files.len(), 1, "只应读入 clips 下的 WebP");
        assert!(pack.files.contains_key(Path::new("clips/wave.webp")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn zip_pack_reads_only_clip_assets() {
        let root = temp_dir("zip-pack");
        let zip_path = root.join("pack.zip");
        {
            let mut writer = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("persona.json", options).unwrap();
            writer.write_all(minimal_persona_json().as_bytes()).unwrap();
            writer.start_file("clips/wave.webp", options).unwrap();
            writer.write_all(&webp_bytes()).unwrap();
            writer.start_file("source.mp4", options).unwrap();
            writer.write_all(&[0u8; 4096]).unwrap();
            writer.finish().unwrap();
        }

        let pack = read_zip_pack(&zip_path).unwrap();
        assert_eq!(pack.files.len(), 1, "只应解压 clips 下的 WebP");
        assert!(pack.files.contains_key(Path::new("clips/wave.webp")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn normalize_relative_path_rejects_unsafe_components() {
        assert_eq!(
            normalize_relative_path(Path::new("clips/wave.webp")).unwrap(),
            PathBuf::from("clips/wave.webp")
        );
        assert!(normalize_relative_path(Path::new("../escape.webp")).is_err());
        assert!(normalize_relative_path(Path::new("clips/../escape.webp")).is_err());
        assert!(normalize_relative_path(Path::new("/etc/passwd")).is_err());
        assert!(normalize_clip_asset("sprites/wave.webp").is_err());
        assert!(normalize_clip_asset("clips/wave.png").is_err());
        assert!(normalize_clip_asset("clips/wave.webp").is_ok());
    }

    #[test]
    fn sanitize_slug_filters_and_falls_back() {
        assert_eq!(sanitize_slug("My Hero!", "名字"), "myhero");
        assert_eq!(sanitize_slug("", "林克"), "character");
        assert_eq!(sanitize_slug("-", ""), "character");
        assert_eq!(sanitize_slug(&"a".repeat(60), "").len(), 40);
    }
}
