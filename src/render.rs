//! Bounded, local CPU rendering. No shell commands, remote media, or paid service.
//!
//! Every invocation owns its process children (`kill_on_drop`) and a private work
//! directory. Cancelling the Tokio task also cancels FFmpeg / the speech engine.
use crate::models::{self, Artifact, Project, Scene, Voice};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const FPS: f64 = 30.0;
const MAX_DURATION: f64 = 180.0;
const PROCESS_LIMIT: Duration = Duration::from_secs(300);
// A file with a misleading .mp4 extension must not be interpreted as a remote
// HLS / DASH playlist. These are the only local input demuxers the studio uses.
const MEDIA_DEMUXERS: &str = "lavfi,concat,mov,matroska,webm,avi,m4v,wav,mp3,ogg,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe,bmp_pipe";

#[derive(Clone)]
struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    voice: Option<PathBuf>,
    piper_model: Option<PathBuf>,
    piper_config: Option<PathBuf>,
}

fn executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn binary(override_key: &str, defaults: &[&str]) -> Option<PathBuf> {
    let override_value = env::var_os(override_key).filter(|v| !v.is_empty());
    let names: Vec<_> = override_value
        .into_iter()
        .chain(
            if env::var_os(override_key)
                .filter(|v| !v.is_empty())
                .is_some()
            {
                Vec::new()
            } else {
                defaults
                    .iter()
                    .map(|name| std::ffi::OsString::from(*name))
                    .collect()
            },
        )
        .collect();
    for name in names {
        let candidate = PathBuf::from(&name);
        if candidate.components().count() > 1 || candidate.is_absolute() {
            if executable(&candidate) {
                return std::fs::canonicalize(candidate).ok();
            }
        } else if let Some(paths) = env::var_os("PATH") {
            for directory in env::split_paths(&paths) {
                let path = directory.join(&candidate);
                if executable(&path) {
                    return std::fs::canonicalize(path).ok();
                }
            }
        }
    }
    None
}

/// This checks installation only. Codec / model compatibility is verified when
/// rendering; an installed Piper binary still needs `PIPER_MODEL` configured.
pub fn capabilities() -> Value {
    let ffmpeg = binary("RUSTCLIP_FFMPEG", &["ffmpeg"]);
    let ffprobe = binary("RUSTCLIP_FFPROBE", &["ffprobe"]);
    let espeak = binary("RUSTCLIP_ESPEAK", &["espeak-ng", "espeak"]);
    let piper = binary("RUSTCLIP_PIPER", &["piper"]);
    let model = env::var_os("PIPER_MODEL").map(PathBuf::from);
    let model_available = model.as_ref().is_some_and(|p| p.is_file());
    let config = env::var_os("PIPER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| model.as_ref().map(|p| p.with_extension("onnx.json")));
    let model_language = config
        .as_ref()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| v["language"]["family"].as_str().map(str::to_owned));
    let preferred = if piper.is_some() && model_available && model_language.as_deref() == Some("ru")
    {
        Voice::Piper
    } else if espeak.is_some() {
        Voice::Espeak
    } else {
        Voice::None
    };
    json!({
        "ffmpeg": ffmpeg.is_some(), "ffprobe": ffprobe.is_some(),
        "espeak": espeak.is_some(), "piper": piper.is_some(),
        "piper_model_available": model_available,
        "piper_language": model_language, "preferred_voice": preferred,
        "paths": { "ffmpeg": ffmpeg, "ffprobe": ffprobe,
            "espeak": espeak, "piper": piper, "piper_model": model,
            "piper_config": env::var_os("PIPER_CONFIG").map(PathBuf::from) },
        "render": { "fps": 30, "max_duration_s": 180, "threads": 2,
            "segment_concurrency": 1, "subtitles": "ASS + SRT",
            "caption_timing": "proportional to text length; not word alignment" }
    })
}

pub fn preferred_voice(language: &str) -> Voice {
    let c = capabilities();
    if c["piper"] == true && c["piper_model_available"] == true && c["piper_language"] == language {
        Voice::Piper
    } else if c["espeak"] == true {
        Voice::Espeak
    } else {
        Voice::None
    }
}

impl Tools {
    fn resolve(voice: Voice) -> Result<Self> {
        let ffmpeg = binary("RUSTCLIP_FFMPEG", &["ffmpeg"])
            .context("Не найден FFmpeg. Установите ffmpeg или задайте RUSTCLIP_FFMPEG")?;
        let ffprobe = binary("RUSTCLIP_FFPROBE", &["ffprobe"])
            .context("Не найден ffprobe. Установите ffmpeg или задайте RUSTCLIP_FFPROBE")?;
        let voice_binary = match voice {
            Voice::None => None,
            Voice::Espeak => Some(binary("RUSTCLIP_ESPEAK", &["espeak-ng", "espeak"])
                .context("Выбран Espeak, но он не установлен. Установите espeak-ng или явно выберите voice: none")?),
            Voice::Piper => Some(binary("RUSTCLIP_PIPER", &["piper"])
                .context("Выбран Piper, но он не установлен. Задайте RUSTCLIP_PIPER или явно выберите другую озвучку")?),
        };
        let (piper_model, piper_config) = if voice == Voice::Piper {
            let path = env::var_os("PIPER_MODEL")
                .map(PathBuf::from)
                .context("Для Piper нужен путь PIPER_MODEL к локальной модели .onnx")?;
            let model = std::fs::canonicalize(&path).context("Модель PIPER_MODEL не найдена")?;
            if !model.is_file() {
                bail!("PIPER_MODEL должен указывать на файл");
            }
            let config = env::var_os("PIPER_CONFIG")
                .map(PathBuf::from)
                .map(|p| std::fs::canonicalize(p).context("PIPER_CONFIG не найден"))
                .transpose()?;
            if config.as_ref().is_some_and(|p| !p.is_file()) {
                bail!("PIPER_CONFIG должен указывать на файл");
            }
            (Some(model), config)
        } else {
            (None, None)
        };
        Ok(Self {
            ffmpeg,
            ffprobe,
            voice: voice_binary,
            piper_model,
            piper_config,
        })
    }
}

async fn drain_bounded<R: AsyncRead + Unpin>(
    mut stream: R,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        let keep = n.min(limit.saturating_sub(retained.len()));
        retained.extend_from_slice(&buffer[..keep]);
        // Continue draining after the bound: otherwise the child's pipe can
        // block forever even though we no longer need its diagnostics.
    }
    Ok(retained)
}

// Keeping the child inside this future is essential: timeouts and cancellation
// drop it, and kill_on_drop terminates it. No orphaned background subprocess.
async fn run(mut cmd: Command, cwd: &Path, input: Option<&str>) -> Result<Vec<u8>> {
    cmd.current_dir(cwd)
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let operation = async {
        let mut child = cmd
            .spawn()
            .context("Не удалось запустить локальный инструмент")?;
        if let Some(text) = input {
            let mut stdin = child.stdin.take().context("Не удалось открыть stdin")?;
            stdin.write_all(text.as_bytes()).await?;
            stdin.shutdown().await?;
            drop(stdin);
        }
        let stdout = child.stdout.take().context("Не удалось открыть stdout")?;
        let stderr = child.stderr.take().context("Не удалось открыть stderr")?;
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            drain_bounded(stdout, 1_048_576),
            drain_bounded(stderr, 65_536)
        )?;
        if !status.success() {
            let diagnostic = String::from_utf8_lossy(&stderr);
            let diagnostic: String = diagnostic.chars().take(6000).collect();
            bail!(
                "Локальный инструмент завершился с {}: {}",
                status,
                diagnostic.trim()
            );
        }
        Ok(stdout)
    };
    tokio::time::timeout(PROCESS_LIMIT, operation)
        .await
        .context("Локальный инструмент превысил лимит 300 секунд и остановлен")?
}

fn ffmpeg_command(tools: &Tools) -> Command {
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args([
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-threads",
        "2",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
    ]);
    cmd
}

fn input(cmd: &mut Command, name: &str) {
    cmd.args([
        "-protocol_whitelist",
        "file,pipe",
        "-format_whitelist",
        MEDIA_DEMUXERS,
        "-i",
        name,
    ]);
}

async fn probe(tools: &Tools, cwd: &Path, name: &str) -> Result<Value> {
    let mut cmd = Command::new(&tools.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-format_whitelist",
        MEDIA_DEMUXERS,
        "-show_entries",
        "format=duration:stream=codec_type,width,height,sample_rate",
        "-of",
        "json",
        name,
    ]);
    let bytes = run(cmd, cwd, None)
        .await
        .context("Не удалось проверить медиа через ffprobe")?;
    serde_json::from_slice(&bytes).context("ffprobe вернул неверный JSON")
}

fn media_duration(probe: &Value) -> Result<f64> {
    let duration = probe["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .context("Медиа не содержит доступной длительности")?;
    if !duration.is_finite() || duration <= 0.0 {
        bail!("Неверная длительность медиа");
    }
    Ok(duration)
}

fn has_stream(probe: &Value, kind: &str) -> bool {
    probe["streams"].as_array().is_some_and(|streams| {
        streams
            .iter()
            .any(|s| s["codec_type"].as_str() == Some(kind))
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum MediaKind {
    Image,
    Video,
    Audio,
}

fn media_kind(extension: &str) -> Result<MediaKind> {
    match extension.to_ascii_lowercase().as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "bmp" => Ok(MediaKind::Image),
        "mp4" | "mov" | "mkv" | "webm" | "m4v" | "avi" => Ok(MediaKind::Video),
        "wav" | "mp3" | "ogg" | "flac" | "m4a" | "aac" | "opus" => Ok(MediaKind::Audio),
        _ => bail!("Формат материала не поддерживается: {extension}"),
    }
}

fn confined_asset(root: &Path, value: &str) -> Result<(PathBuf, MediaKind, String)> {
    // Path::components normalizes `.` on Unix; inspect raw pieces as well.
    if value
        .split('/')
        .any(|p| p.is_empty() || p == "." || p == "..")
        || value.contains('\\')
        || !value.starts_with("assets/")
        || Path::new(value)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("Материалы должны быть относительными путями внутри assets/");
    }
    let asset_root =
        std::fs::canonicalize(root.join("assets")).context("Папка assets не найдена")?;
    if !asset_root.starts_with(root) {
        bail!("Папка assets выходит за пределы каталога студии");
    }
    let asset = std::fs::canonicalize(root.join(value)).context("Материал не найден")?;
    if !asset.starts_with(&asset_root) || !asset.is_file() {
        bail!("Материал выходит за пределы assets/");
    }
    let extension = Path::new(value)
        .extension()
        .and_then(OsStr::to_str)
        .context("Материалу нужно расширение файла")?
        .to_ascii_lowercase();
    let kind = media_kind(&extension)?;
    Ok((asset, kind, extension))
}

async fn prepare_asset(
    root: &Path,
    work: &Path,
    value: &str,
    stem: &str,
    expect_audio: bool,
    tools: &Tools,
) -> Result<(String, MediaKind)> {
    let (source, kind, extension) = confined_asset(root, value)?;
    if expect_audio && kind == MediaKind::Image {
        bail!("Для музыки нужен аудио- или видеофайл");
    }
    if !expect_audio && kind == MediaKind::Audio {
        bail!("Для сцены нужен рисунок или видео");
    }
    // All later filenames are generated by us, so no user text reaches FFmpeg
    // filter expressions or concat's file parser.
    let name = format!("{stem}.{extension}");
    fs::copy(&source, work.join(&name))
        .await
        .context("Не удалось скопировать материал")?;
    let inspected = probe(tools, work, &name).await?;
    let required = if expect_audio { "audio" } else { "video" };
    if !has_stream(&inspected, required) {
        bail!("В материале {value} нет потока {required}");
    }
    Ok((name, kind))
}

/// ASS has its own command language, separate from the shell. Use fullwidth
/// literal punctuation for its three metacharacters so no user override runs.
fn ass_literal(text: &str) -> String {
    let mut safe = String::new();
    for ch in text.chars() {
        match ch {
            '\\' => safe.push('＼'),
            '{' => safe.push('｛'),
            '}' => safe.push('｝'),
            '\n' => safe.push_str("\\N"),
            '\r' => {}
            ch if ch.is_control() => safe.push(' '),
            ch => safe.push(ch),
        }
    }
    safe
}

fn wrapped(text: &str, line_limit: usize) -> String {
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > line_limit {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            // Very long URLs or unbroken words must not run beyond the canvas.
            for ch in word.chars() {
                if line.chars().count() >= line_limit {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(ch);
            }
        }
        if !line.trim().is_empty() {
            lines.push(line.trim_end().to_owned());
        }
    }
    lines.join("\n")
}

fn caption_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.chars().count() + word.chars().count() + 1 > max_chars {
            result.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

#[derive(Clone, Debug)]
struct Caption {
    start: f64,
    end: f64,
    text: String,
}

fn captions(
    text: &str,
    duration: f64,
    audible_duration: Option<f64>,
    max_chars: usize,
) -> Vec<Caption> {
    let chunks = caption_chunks(text, max_chars);
    let weight: usize = chunks.iter().map(|s| s.chars().count()).sum();
    if weight == 0 {
        return Vec::new();
    }
    let end = audible_duration.unwrap_or(duration).min(duration).max(0.1);
    let mut elapsed = 0.0;
    chunks
        .into_iter()
        .map(|text| {
            let start = elapsed;
            elapsed += end * text.chars().count() as f64 / weight as f64;
            Caption {
                start,
                end: elapsed.min(end),
                text,
            }
        })
        .collect()
}

fn ass_time(seconds: f64) -> String {
    let t = (seconds.max(0.0) * 100.0).round() as u64;
    format!(
        "{}:{:02}:{:02}.{:02}",
        t / 360_000,
        t / 6000 % 60,
        t / 100 % 60,
        t % 100
    )
}

fn srt_time(seconds: f64) -> String {
    let t = (seconds.max(0.0) * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        t / 3_600_000,
        t / 60_000 % 60,
        t / 1000 % 60,
        t % 1000
    )
}

fn scene_ass(
    scene: &Scene,
    index: usize,
    count: usize,
    dimensions: (u32, u32),
    duration: f64,
    dark: bool,
    caption_list: &[Caption],
) -> String {
    let (width, height) = dimensions;
    let portrait = height > width;
    let scale = width as f64 / 1080.0;
    let title_text = wrapped(
        &scene.title,
        if portrait || width == height { 20 } else { 38 },
    );
    let body_text = wrapped(
        &scene.text,
        if portrait || width == height { 31 } else { 60 },
    );
    let title_size = (if portrait {
        88.0
    } else {
        66.0 * scale.min(1.5)
    })
    .min(height as f64 * 0.25 / (title_text.lines().count().max(1) as f64 * 1.2));
    let body_size = (if portrait {
        49.0
    } else {
        40.0 * scale.min(1.4)
    })
    .min(height as f64 * 0.25 / (body_text.lines().count().max(1) as f64 * 1.2));
    let caption_size = if portrait {
        46.0
    } else {
        36.0 * scale.min(1.4)
    };
    let color = if dark { "&H00F4F2EB" } else { "&H00241D17" };
    let title = ass_literal(&title_text);
    let body = ass_literal(&body_text);
    let font = "DejaVu Sans";
    let x = width / 2;
    let title_y = (height as f64 * 0.29) as u32;
    let body_y = (height as f64 * 0.56) as u32;
    let caption_y = (height as f64 * 0.80) as u32;
    let top = (height as f64 * 0.07) as u32;
    let bottom = (height as f64 * 0.91) as u32;
    let end = ass_time(duration);
    let mut ass = format!(
        r#"[Script Info]
ScriptType: v4.00+
PlayResX: {width}
PlayResY: {height}
WrapStyle: 2
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Title,{font},{title_size},{color},{color},&H00000000,&H00000000,-1,0,0,0,100,100,0,0,1,0,0,5,70,70,0,1
Style: Body,{font},{body_size},{color},{color},&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,5,70,70,0,1
Style: Small,{font},27,{color},{color},&H00000000,&H00000000,0,0,0,0,100,100,2,0,1,0,0,5,40,40,0,1
Style: Caption,{font},{caption_size},&H00FFFFFF,&H00FFFFFF,&H001C1714,&H001C1714,0,0,0,0,100,100,0,0,3,16,0,5,60,60,0,1
Style: Shape,{font},20,&H00A4F5D0,&H00A4F5D0,&H00A4F5D0,&H00A4F5D0,0,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"#
    );
    // Decorative drawing, type motion, and progress are generated instructions.
    ass.push_str(&format!("Dialogue: 0,0:00:00.00,{end},Shape,,0,0,0,,{{\\an7\\move(-110,{bottom},-30,{bottom})\\1c&H0074B5FA&\\p1}}m 0 0 l 470 0 470 90 0 90\n"));
    ass.push_str(&format!("Dialogue: 0,0:00:00.00,{end},Shape,,0,0,0,,{{\\an7\\move({},0,{},80)\\1c&H00A4F5D0&\\p1}}m 0 0 l 180 0 180 210 0 210\n", width - 200, width - 230));
    ass.push_str(&format!("Dialogue: 1,0:00:00.00,{end},Small,,0,0,0,,{{\\pos({x},{top})\\fad(250,200)}}RUSTCLIP  /  MINI STUDIO\n"));
    ass.push_str(&format!("Dialogue: 1,0:00:00.00,{end},Title,,0,0,0,,{{\\move({x},{},{x},{title_y},0,650)\\fad(250,200)}}{title}\n", title_y + 35));
    ass.push_str(&format!(
        "Dialogue: 1,0:00:00.00,{end},Body,,0,0,0,,{{\\pos({x},{body_y})\\fad(650,200)}}{body}\n"
    ));
    ass.push_str(&format!("Dialogue: 1,0:00:00.00,{end},Small,,0,0,0,,{{\\pos({x},{bottom})\\fad(250,200)}}{:02}  /  {:02}\n", index + 1, count));
    for caption in caption_list {
        let text = ass_literal(&wrapped(&caption.text, if portrait { 31 } else { 65 }));
        ass.push_str(&format!(
            "Dialogue: 3,{},{},Caption,,0,0,0,,{{\\pos({x},{caption_y})}}{text}\n",
            ass_time(caption.start),
            ass_time(caption.end)
        ));
    }
    ass
}

fn srt(captions: &[Caption]) -> String {
    let mut output = String::new();
    for (i, caption) in captions.iter().enumerate() {
        let text = caption
            .text
            .replace(['\r', '\n'], " ")
            .replace('<', "＜")
            .replace('>', "＞");
        output.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            srt_time(caption.start),
            srt_time(caption.end),
            text
        ));
    }
    output
}

fn frame_duration(requested: f64, voice_duration: Option<f64>) -> f64 {
    let duration = requested.max(voice_duration.map_or(0.0, |d| d + 0.35));
    (duration * FPS).ceil() / FPS
}

struct Cleanup {
    path: PathBuf,
    committed: bool,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

async fn digest(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 65536];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Render a snapshot of a project. Persistence and state transitions are owned
/// by the job worker; this function returns only after all outputs are verified.
pub async fn render(project: &Project, data_dir: &Path) -> Result<Artifact> {
    project.spec.validate()?;
    if project.id.is_empty()
        || project.id.len() > 128
        || !project
            .id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Неверный идентификатор проекта");
    }
    let tools = Tools::resolve(project.spec.voice)?;
    fs::create_dir_all(data_dir).await?;
    let root = fs::canonicalize(data_dir).await?;
    fs::create_dir_all(root.join("assets")).await?;
    let artifact_id = models::id();
    let relative = format!("renders/{}/{}", project.id, artifact_id);
    let parent = root.join("renders").join(&project.id);
    fs::create_dir_all(&parent).await?;
    let canonical_parent = fs::canonicalize(&parent).await?;
    if !canonical_parent.starts_with(&root) {
        bail!("Каталог рендера выходит за пределы студии");
    }
    let output = canonical_parent.join(&artifact_id);
    fs::create_dir(&output).await?;
    let mut cleanup = Cleanup {
        path: output.clone(),
        committed: false,
    };
    let work = output.join("work");
    fs::create_dir(&work).await?;
    let (width, height) = project.spec.profile.dimensions();
    let mut durations = Vec::new();
    let mut voices = Vec::new();
    let mut global_captions = Vec::new();
    let mut elapsed = 0.0;
    // Speech first: do not waste encoding work if narration extends past limit.
    for (i, scene) in project.spec.scenes.iter().enumerate() {
        let voice_name = format!("voice-{i:02}.wav");
        let voice_duration = if project.spec.voice != Voice::None
            && !scene.narration.trim().is_empty()
        {
            let mut cmd = Command::new(tools.voice.as_ref().context("Не выбран движок озвучки")?);
            match project.spec.voice {
                Voice::Espeak => {
                    let name = format!("narration-{i:02}.txt");
                    fs::write(work.join(&name), &scene.narration).await?;
                    cmd.args([
                        "-v",
                        &project.spec.language,
                        "-s",
                        "155",
                        "-w",
                        &voice_name,
                        "-f",
                        &name,
                    ]);
                    run(cmd, &work, None)
                        .await
                        .context("Ошибка озвучки eSpeak")?;
                }
                Voice::Piper => {
                    cmd.arg("--model")
                        .arg(
                            tools
                                .piper_model
                                .as_ref()
                                .context("Не задана модель Piper")?,
                        )
                        .args(["--output_file", &voice_name]);
                    if let Some(config) = &tools.piper_config {
                        cmd.arg("--config").arg(config);
                    }
                    // ONNX workers otherwise use all host cores.
                    cmd.env("OMP_NUM_THREADS", "2")
                        .env("OPENBLAS_NUM_THREADS", "2");
                    run(cmd, &work, Some(&format!("{}\n", scene.narration)))
                        .await
                        .context("Ошибка озвучки Piper")?;
                }
                Voice::None => unreachable!(),
            }
            let p = probe(&tools, &work, &voice_name).await?;
            if !has_stream(&p, "audio") {
                bail!("Движок озвучки не создал аудио");
            }
            Some(media_duration(&p)?)
        } else {
            None
        };
        let duration = frame_duration(scene.duration_s, voice_duration);
        elapsed += duration;
        if elapsed > MAX_DURATION + 0.0001 {
            bail!("После озвучки ролик длится {elapsed:.1} с; лимит 180 с. Сократите narration или число сцен");
        }
        voices.push(voice_duration.map(|_| voice_name));
        durations.push((duration, voice_duration));
    }
    let music = if let Some(value) = &project.spec.music_asset {
        Some(
            prepare_asset(&root, &work, value, "music", true, &tools)
                .await?
                .0,
        )
    } else {
        None
    };
    let mut offset = 0.0;
    let mut concat = String::new();
    for (i, scene) in project.spec.scenes.iter().enumerate() {
        let (duration, voice_duration) = durations[i];
        let local_captions = captions(
            &scene.narration,
            duration,
            voice_duration,
            if height > width { 57 } else { 92 },
        );
        global_captions.extend(local_captions.iter().map(|c| Caption {
            start: c.start + offset,
            end: c.end + offset,
            text: c.text.clone(),
        }));
        let asset = if let Some(value) = &scene.asset {
            Some(prepare_asset(&root, &work, value, &format!("asset-{i:02}"), false, &tools).await?)
        } else {
            None
        };
        let dark = asset.is_some() || i % 3 == 2;
        let ass_name = format!("scene-{i:02}.ass");
        fs::write(
            work.join(&ass_name),
            scene_ass(
                scene,
                i,
                project.spec.scenes.len(),
                (width, height),
                duration,
                dark,
                &local_captions,
            ),
        )
        .await?;
        let mut cmd = ffmpeg_command(&tools);
        match &asset {
            Some((name, MediaKind::Image)) => {
                cmd.args(["-loop", "1", "-framerate", "30"]);
                input(&mut cmd, name);
            }
            Some((name, MediaKind::Video)) => {
                cmd.args(["-stream_loop", "-1"]);
                input(&mut cmd, name);
            }
            Some((_, MediaKind::Audio)) => unreachable!(),
            None => {
                let color = if dark {
                    "0x181D24"
                } else if i % 3 == 1 {
                    "0xE6E9F5"
                } else {
                    "0xF3EFE5"
                };
                cmd.args(["-f", "lavfi"]);
                input(
                    &mut cmd,
                    &format!("color=c={color}:s={width}x{height}:r=30:d={duration:.6}"),
                );
            }
        }
        if let Some(name) = &voices[i] {
            input(&mut cmd, name);
        } else {
            cmd.args(["-f", "lavfi"]);
            input(&mut cmd, "anullsrc=r=48000:cl=stereo");
        }
        let asset_overlay = if asset.is_some() {
            // RGB multiplication preserves asset colors. Alpha drawbox on
            // subsampled YUV repeatedly blends shared chroma samples and can
            // wash a saturated image almost gray (covered by integration test).
            ",format=rgb24,colorchannelmixer=rr=0.36:gg=0.36:bb=0.36,format=yuv420p"
        } else {
            ""
        };
        // Piper normalizes to full scale; leave headroom for AAC encoding.
        let voice_gain = if project.spec.voice == Voice::Piper {
            ",volume=0.85"
        } else {
            ""
        };
        let filter = format!("[0:v]scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height},setsar=1,fps=30,format=yuv420p{asset_overlay},ass={ass_name}[v];[1:a]aresample=48000,aformat=channel_layouts=stereo{voice_gain},apad,atrim=0:{duration:.6},afade=t=in:st=0:d=0.03,afade=t=out:st={:.6}:d=0.12[a]", (duration - 0.12).max(0.0));
        let segment = format!("scene-{i:02}.mp4");
        cmd.args([
            "-filter_complex",
            &filter,
            "-map",
            "[v]",
            "-map",
            "[a]",
            "-t",
            &format!("{duration:.6}"),
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-r",
            "30",
            "-threads",
            "2",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-movflags",
            "+faststart",
            &segment,
        ]);
        run(cmd, &work, None)
            .await
            .with_context(|| format!("Не удалось смонтировать сцену {}", i + 1))?;
        concat.push_str(&format!("file '{segment}'\nduration {duration:.6}\n"));
        offset += duration;
    }
    fs::write(work.join("concat.txt"), concat).await?;
    let mut cmd = ffmpeg_command(&tools);
    let concat_target = if music.is_some() {
        work.join("joined.mp4")
    } else {
        output.join("video.mp4")
    };
    cmd.args(["-f", "concat", "-safe", "1"]);
    input(&mut cmd, "concat.txt");
    cmd.args([
        "-map",
        "0:v:0",
        "-map",
        "0:a:0",
        "-c",
        "copy",
        "-movflags",
        "+faststart",
    ])
    .arg(&concat_target);
    run(cmd, &work, None)
        .await
        .context("Не удалось объединить сцены")?;
    if let Some(name) = music {
        let mut cmd = ffmpeg_command(&tools);
        let mix = format!("[0:a]aresample=48000,aformat=channel_layouts=stereo[voice];[1:a]aresample=48000,aformat=channel_layouts=stereo,volume=0.12,atrim=0:{offset:.6},afade=t=in:st=0:d=0.5,afade=t=out:st={:.6}:d=0.5[music];[voice][music]amix=inputs=2:duration=first:dropout_transition=0:normalize=0,alimiter=limit=0.94[a]", (offset - 0.5).max(0.0));
        input(&mut cmd, "joined.mp4");
        cmd.args(["-stream_loop", "-1"]);
        input(&mut cmd, &name);
        cmd.args([
            "-filter_complex",
            &mix,
            "-map",
            "0:v:0",
            "-map",
            "[a]",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-t",
            &format!("{offset:.6}"),
            "-movflags",
            "+faststart",
        ])
        .arg(output.join("video.mp4"));
        run(cmd, &work, None)
            .await
            .context("Не удалось свести музыку с голосом")?;
    }
    let inspected = probe(&tools, &output, "video.mp4").await?;
    let actual = media_duration(&inspected)?;
    let video = inspected["streams"]
        .as_array()
        .and_then(|streams| streams.iter().find(|s| s["codec_type"] == "video"))
        .context("В готовом файле нет видео")?;
    if video["width"].as_u64() != Some(width as u64)
        || video["height"].as_u64() != Some(height as u64)
        || !has_stream(&inspected, "audio")
        || (actual - offset).abs() > 0.20
    {
        bail!("Готовый файл не прошёл проверку размеров, аудио или длительности");
    }
    let mut cmd = ffmpeg_command(&tools);
    cmd.args(["-ss", &format!("{:.3}", (durations[0].0 * 0.45).min(1.8))]);
    input(&mut cmd, "video.mp4");
    cmd.args(["-frames:v", "1", "-q:v", "2", "-update", "1", "poster.jpg"]);
    run(cmd, &output, None)
        .await
        .context("Не удалось создать обложку")?;
    if fs::metadata(output.join("poster.jpg")).await?.len() == 0 {
        bail!("Обложка не создана");
    }
    fs::write(output.join("captions.srt"), srt(&global_captions)).await?;
    let artifact = Artifact {
        id: artifact_id,
        path: format!("{relative}/video.mp4"),
        width,
        height,
        duration_s: actual,
        revision: project.revision,
        sha256: digest(&output.join("video.mp4")).await?,
        created_at: models::now(),
    };
    let metadata = json!({
        "artifact": artifact, "project_id": project.id, "project_revision": project.revision,
        "renderer": "rustclip/ffmpeg-cpu-v1", "fps": 30,
        "voice": project.spec.voice, "language": project.spec.language,
        "scene_durations_s": durations.iter().map(|d| d.0).collect::<Vec<_>>(),
        "caption_timing": "proportional text-length approximation, not forced alignment",
        "source_urls": project.spec.source_urls,
        "scene_assets": project.spec.scenes.iter().map(|s| &s.asset).collect::<Vec<_>>(),
        "music_asset": project.spec.music_asset,
        "files": { "video": "video.mp4", "poster": "poster.jpg", "captions": "captions.srt" }
    });
    fs::write(
        output.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )
    .await?;
    fs::remove_dir_all(&work).await?;
    cleanup.committed = true;
    Ok(artifact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Profile, ProjectSpec};

    #[test]
    fn ass_input_cannot_add_overrides_or_dialogue_lines() {
        let escaped = ass_literal("Привет {\\pos(0,0)}\r\nDialogue: 5,0:00:00.00\\N");
        assert!(!escaped.contains('{'));
        assert!(!escaped.contains('}'));
        assert!(!escaped.contains("\\pos"));
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains('\r'));
        assert!(escaped.contains("\\NDialogue:"));
    }

    #[test]
    fn timeline_extends_voice_and_never_loses_last_caption() {
        assert!((frame_duration(2.0, Some(3.0)) - 3.3666666667).abs() < 0.001);
        let list = captions(
            "Раз два три четыре пять шесть семь восемь",
            5.0,
            Some(4.2),
            12,
        );
        assert!(list.len() > 2);
        assert_eq!(list[0].start, 0.0);
        assert!((list.last().unwrap().end - 4.2).abs() < 0.0001);
        for pair in list.windows(2) {
            assert!((pair[0].end - pair[1].start).abs() < 0.0001);
        }
        assert_eq!(srt_time(61.123), "00:01:01,123");
        assert_eq!(ass_time(61.12), "0:01:01.12");
        assert_eq!(wrapped("Один два три", 8), "Один два\nтри");
        assert!(wrapped("Оченьдлинноеслитноесловобезпробелов", 8)
            .lines()
            .all(|line| line.chars().count() <= 8));
    }

    #[test]
    fn paths_and_extensions_are_checked_before_ffmpeg() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/a.png"), b"test").unwrap();
        assert_eq!(
            confined_asset(&root, "assets/a.png").unwrap().1,
            MediaKind::Image
        );
        for value in [
            "assets/../a.png",
            "assets/./a.png",
            "assets//a.png",
            "assets\\a.png",
            "/etc/passwd",
            "file:https://a.test",
            "assets/missing.png",
        ] {
            assert!(confined_asset(&root, value).is_err(), "{value}");
        }
        std::fs::write(root.join("assets/a.html"), b"test").unwrap();
        assert!(confined_asset(&root, "assets/a.html").is_err());
        #[cfg(unix)]
        {
            std::fs::write(root.join("outside.png"), b"test").unwrap();
            std::os::unix::fs::symlink(root.join("outside.png"), root.join("assets/link.png"))
                .unwrap();
            assert!(confined_asset(&root, "assets/link.png").is_err());
        }
    }

    #[tokio::test]
    #[ignore = "requires installed FFmpeg with libass/libx264; renders a real one-second Full HD file"]
    async fn real_ffmpeg_silent_render_has_verified_artifact_and_subtitles() {
        let temp = tempfile::tempdir().unwrap();
        let project = Project {
            id: "renderer-test".into(),
            revision: 7,
            created_at: models::now(),
            updated_at: models::now(),
            status: "draft".into(),
            artifact: None,
            error: None,
            spec: ProjectSpec {
                title: "Проверка монтажа".into(),
                description: String::new(),
                topic: String::new(),
                profile: Profile::Shorts,
                language: "ru".into(),
                voice: Voice::None,
                scenes: vec![Scene {
                    title: "Один ясный шаг".into(),
                    text: "Русские титры, графика и монтаж".into(),
                    narration: "Первый законченный результат".into(),
                    duration_s: 1.0,
                    asset: None,
                }],
                music_asset: None,
                source_urls: vec![],
                tags: vec![],
            },
        };
        let artifact = render(&project, temp.path()).await.unwrap();
        assert_eq!(artifact.width, 1080);
        assert_eq!(artifact.height, 1920);
        assert_eq!(artifact.revision, 7);
        assert!((artifact.duration_s - 1.0).abs() < 0.1);
        let path = temp.path().join(&artifact.path);
        assert!(path.is_file());
        assert_eq!(artifact.sha256, digest(&path).await.unwrap());
        let dir = path.parent().unwrap();
        assert!(fs::metadata(dir.join("poster.jpg")).await.unwrap().len() > 1000);
        let subtitles = fs::read_to_string(dir.join("captions.srt")).await.unwrap();
        assert!(subtitles.contains("Первый законченный результат"));
        assert!(subtitles.contains("00:00:00,000 --> 00:00:01,000"));
        assert!(!dir.join("work").exists());
    }

    #[tokio::test]
    #[ignore = "requires installed FFmpeg; generates local image/video/music and renders a real two-scene landscape file"]
    async fn real_ffmpeg_landscape_assets_concat_and_music_mix() {
        let temp = tempfile::tempdir().unwrap();
        let tools = Tools::resolve(Voice::None).unwrap();
        let assets = temp.path().join("assets");
        fs::create_dir(&assets).await.unwrap();

        // All fixtures are generated locally, bounded to one frame or <3 sec.
        // Different source aspect ratios exercise both scale and crop paths.
        let mut cmd = ffmpeg_command(&tools);
        cmd.args(["-f", "lavfi"]);
        input(&mut cmd, "color=c=0xE04545:s=640x360:r=30:d=0.1");
        cmd.args([
            "-frames:v",
            "1",
            "-c:v",
            "png",
            "-threads",
            "2",
            "-update",
            "1",
            "wide.png",
        ]);
        run(cmd, &assets, None).await.unwrap();

        let mut cmd = ffmpeg_command(&tools);
        cmd.args(["-f", "lavfi"]);
        input(&mut cmd, "color=c=0x3160CE:s=180x320:r=30:d=0.4");
        cmd.args([
            "-an", "-t", "0.4", "-c:v", "libx264", "-preset", "veryfast", "-threads", "2",
            "-pix_fmt", "yuv420p", "tall.mp4",
        ]);
        run(cmd, &assets, None).await.unwrap();

        let mut cmd = ffmpeg_command(&tools);
        cmd.args(["-f", "lavfi"]);
        input(
            &mut cmd,
            "sine=frequency=440:sample_rate=48000:duration=2.3",
        );
        cmd.args(["-t", "2.3", "-c:a", "pcm_s16le", "tone.wav"]);
        run(cmd, &assets, None).await.unwrap();

        let project = Project {
            id: "renderer-landscape-test".into(),
            revision: 3,
            created_at: models::now(),
            updated_at: models::now(),
            status: "draft".into(),
            artifact: None,
            error: None,
            spec: ProjectSpec {
                title: "Две сцены с собственными материалами".into(),
                description: String::new(),
                topic: String::new(),
                profile: Profile::Landscape,
                language: "ru".into(),
                voice: Voice::None,
                scenes: vec![
                    Scene {
                        title: "Первый кадр".into(),
                        text: "Локальное изображение".into(),
                        narration: "Красный фон первой сцены".into(),
                        duration_s: 1.0,
                        asset: Some("assets/wide.png".into()),
                    },
                    Scene {
                        title: "Второй кадр".into(),
                        text: "Локальный вертикальный клип".into(),
                        narration: "Синий фон второй сцены".into(),
                        duration_s: 1.0,
                        asset: Some("assets/tall.mp4".into()),
                    },
                ],
                music_asset: Some("assets/tone.wav".into()),
                source_urls: vec![],
                tags: vec![],
            },
        };
        let artifact = render(&project, temp.path()).await.unwrap();
        assert_eq!((artifact.width, artifact.height), (1920, 1080));
        assert_eq!(artifact.revision, 3);
        assert!((artifact.duration_s - 2.0).abs() < 0.15);
        let video = temp.path().join(&artifact.path);
        let output = video.parent().unwrap();
        assert_eq!(artifact.sha256, digest(&video).await.unwrap());

        let mut cmd = Command::new(&tools.ffprobe);
        cmd.args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,nb_read_frames",
            "-of",
            "json",
            "video.mp4",
        ]);
        let decoded: Value =
            serde_json::from_slice(&run(cmd, output, None).await.unwrap()).unwrap();
        assert_eq!(decoded["streams"][0]["width"], 1920);
        assert_eq!(decoded["streams"][0]["height"], 1080);
        let frames: u32 = decoded["streams"][0]["nb_read_frames"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(frames, 60, "concat must preserve both thirty-frame scenes");

        // A corner below branding and outside title proves the respective
        // assets survived scaling/cropping and the temporal scene transition.
        let mut pixels = Vec::new();
        for time in ["0.25", "1.25"] {
            let mut cmd = ffmpeg_command(&tools);
            cmd.args(["-ss", time]);
            input(&mut cmd, "video.mp4");
            cmd.args([
                "-vf",
                "format=rgb24,crop=2:2:480:108",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "pipe:1",
            ]);
            let rgb = run(cmd, output, None).await.unwrap();
            assert_eq!(rgb.len(), 12);
            pixels.push((rgb[0], rgb[2]));
        }
        assert!(
            pixels[0].0 > pixels[0].1.saturating_add(20),
            "first scene must retain its red source: {pixels:?}"
        );
        assert!(
            pixels[1].1 > pixels[1].0.saturating_add(20),
            "second scene must retain its blue source: {pixels:?}"
        );

        let mut cmd = ffmpeg_command(&tools);
        input(&mut cmd, "video.mp4");
        cmd.args([
            "-map",
            "0:a:0",
            "-t",
            "2.0",
            "-ar",
            "8000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ]);
        let pcm = run(cmd, output, None).await.unwrap();
        assert!(pcm.len() >= 30_000 && pcm.len() <= 32_000);
        let peak = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .unwrap();
        assert!(
            peak > 50,
            "voice=None leaves silence, so nonzero audio proves music was mixed"
        );
        let subtitles = fs::read_to_string(output.join("captions.srt"))
            .await
            .unwrap();
        assert!(subtitles.contains("00:00:00,000 --> 00:00:01,000\nКрасный фон первой сцены"));
        assert!(subtitles.contains("00:00:01,000 --> 00:00:02,000\nСиний фон второй сцены"));
        let metadata: Value =
            serde_json::from_slice(&fs::read(output.join("metadata.json")).await.unwrap()).unwrap();
        assert_eq!(metadata["scene_durations_s"], json!([1.0, 1.0]));
        assert_eq!(metadata["music_asset"], "assets/tone.wav");
        assert!(!output.join("work").exists());
    }
}
