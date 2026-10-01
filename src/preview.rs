/*
 * 预览区渲染：图片 (kitty 直通 / chafa)、视频、PDF、音频、文本文件、目录、压缩包、多文件、剪贴板文本。
 * bat / eza / bsdtar / pdftoppm / ffprobe / ffmpeg / jq 都是可选依赖：装了才用，没装就退回纯文本。
 * 输出顺序统一是「文字信息在上、图片在下」：kitty 图片会盖住同一区域的文字，放下面就不会互相遮挡。
 */

use crate::{cache_dir, ext_of, get_mime_from_path, have, html_to_text, is_html, is_video_path, looks_binary, touch, TEXT_PREVIEW_LIMIT};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

const AUDIO_EXTS: &[&str] = &["mp3", "flac", "m4a", "ogg", "opus", "wav", "aac", "wma", "ape"];
const GRAY: &str = "\x1b[90m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

static RE_HEX_COLOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$").unwrap());

/// 预览区可用的列数、行数
#[derive(Clone, Copy)]
pub struct Area { pub cols: u32, pub rows: u32, top: u32 }

impl Area {
    pub fn from_env() -> Self {
        let get = |k: &str, d: u32| env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        Area { cols: get("FZF_PREVIEW_COLUMNS", 80), rows: get("FZF_PREVIEW_LINES", 24), top: 0 }
    }
    // 扣掉上方已经输出的 n 行文字后剩下的区域
    fn below(self, n: usize) -> Self {
        Area { cols: self.cols, rows: self.rows.saturating_sub(n as u32).max(3), top: self.top + n as u32 }
    }
}

// ---------- 小工具 ----------

fn char_width(c: char) -> usize {
    match c as u32 {
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD => 2,
        _ => 1,
    }
}

// 按显示宽度截断，避免 fzf 自动换行把下面的图片挤出预览区
fn fit(s: &str, cols: u32) -> String {
    let max = cols.max(4) as usize;
    if s.chars().map(char_width).sum::<usize>() <= max { return s.to_string(); }
    let mut w = 0;
    let mut out = String::new();
    for c in s.chars() {
        if w + char_width(c) > max - 1 { break; }
        w += char_width(c);
        out.push(c);
    }
    out.push('…');
    out
}

// 从左边截断（路径保留文件名那一端）
fn fit_left(s: &str, cols: u32) -> String {
    let max = cols.max(4) as usize;
    if s.chars().map(char_width).sum::<usize>() <= max { return s.to_string(); }
    let mut w = 0;
    let mut tail: Vec<char> = Vec::new();
    for c in s.chars().rev() {
        if w + char_width(c) > max - 1 { break; }
        w += char_width(c);
        tail.push(c);
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

fn short_path(p: &str) -> String {
    match dirs::home_dir().map(|h| h.to_string_lossy().into_owned()) {
        Some(home) if p.starts_with(&home) && p[home.len()..].starts_with('/') => format!("~{}", &p[home.len()..]),
        _ => p.to_string(),
    }
}

fn print_header(lines: &[String], area: Area) {
    for (i, l) in lines.iter().enumerate() {
        let style = if i == 0 { BOLD } else { GRAY };
        println!("{style}{}{RESET}", fit(l, area.cols));
    }
}

fn human_size(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", units[i]) }
}

fn human_duration(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn file_len(path: &str) -> u64 { fs::metadata(path).map(|m| m.len()).unwrap_or(0) }

// 缓存键：路径 + 大小 + 修改时间，文件被替换后缩略图会自动失效
fn file_key(path: &str) -> String {
    let meta = fs::metadata(path).ok();
    let mtime = meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let len = meta.map(|m| m.len()).unwrap_or(0);
    format!("{:x}", xxhash_rust::xxh3::xxh3_128(format!("{path}\0{len}\0{mtime}").as_bytes()))
}

// 生成型缓存：先写 .part 再改名，生成失败不留下 0 字节文件（否则以后永远不会重试）
fn cached(cache_dir: &Path, key: &str, ext: &str, generate: impl FnOnce(&Path) -> bool) -> Option<PathBuf> {
    let file = cache_dir.join(format!("{key}.{ext}"));
    if fs::metadata(&file).map(|m| m.len() > 0).unwrap_or(false) { touch(&file); return Some(file); }
    let part = cache_dir.join(format!("{key}.part.{ext}"));
    let ok = generate(&part) && fs::metadata(&part).map(|m| m.len() > 0).unwrap_or(false);
    if ok && fs::rename(&part, &file).is_ok() { return Some(file); }
    let _ = fs::remove_file(&part);
    None
}

fn cached_text(cache_dir: &Path, key: &str, generate: impl FnOnce() -> Option<String>) -> Option<String> {
    let file = cache_dir.join(format!("{key}.info"));
    if let Ok(s) = fs::read_to_string(&file) { touch(&file); return Some(s); }
    let s = generate()?;
    crate::write_atomic(&file, s.as_bytes());
    Some(s)
}

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

// 跑命令，只取前 max_lines 行输出就杀掉（大压缩包、大目录不用等它跑完）
fn head_of(cmd: &mut Command, max_lines: usize) -> Option<(Vec<String>, bool)> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut lines = Vec::new();
    let mut more = false;
    if let Some(out) = child.stdout.take() {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if lines.len() >= max_lines { more = true; break; }
            lines.push(line);
        }
    }
    let _ = child.kill();
    let status = child.wait().ok()?;
    if lines.is_empty() && !status.success() { return None; }
    Some((lines, more))
}

fn ffprobe(path: &str, entries: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if !have("ffprobe") { return map; }
    if let Ok(out) = Command::new("ffprobe").args(["-v", "error", "-show_entries", entries, "-of", "default=nw=1"]).arg(path).stderr(Stdio::null()).output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim_start_matches("TAG:").to_lowercase();
                if !v.is_empty() && v != "N/A" { map.entry(k).or_insert_with(|| v.to_string()); }
            }
        }
    }
    map
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() { out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char); } else { out.push('='); }
        }
    }
    out
}

// ---------- 图片 ----------

fn png_size(path: &Path) -> Option<(u32, u32)> {
    let mut head = [0u8; 24];
    fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" { return None; }
    let w = u32::from_be_bytes(head[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(head[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

// kitty 直通：只把文件路径发给终端 (t=f)，由 kitty 自己解码 PNG。
// 比 chafa 解码+缩放+把约 2MB 的 RGBA base64 塞进 fzf 快一个数量级。
fn kitty_png(path: &Path, (w, h): (u32, u32), area: Area) -> bool {
    let Ok(abs) = fs::canonicalize(path) else { return false };
    let (cw, ch) = match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 =>
            (ws.width as f64 / ws.columns as f64, ws.height as f64 / ws.rows as f64),
        _ => (10.0, 20.0),
    };
    // 和 chafa 一样等比缩放到填满预览区（小图也放大）
    let scale = (area.cols as f64 * cw / w as f64).min(area.rows as f64 * ch / h as f64);
    let c = ((w as f64 * scale / cw).round() as u32).clamp(1, area.cols);
    let r = ((h as f64 * scale / ch).round() as u32).clamp(1, area.rows);
    let payload = base64(abs.to_string_lossy().as_bytes());
    let mut out = std::io::stdout().lock();
    write!(out, "\x1b_Ga=T,f=100,t=f,c={c},r={r},q=2;{payload}\x1b\\").is_ok() && out.flush().is_ok()
}

fn render_image(path: &Path, area: Area) {
    let is_kitty = env::var("ENABLE_ICAT").unwrap_or_default() == "1";
    let is_native = env::var("IS_NATIVE_KITTY").unwrap_or_default() == "1";
    let size = format!("{}x{}", area.cols, area.rows);
    let mime = get_mime_from_path(path);

    // kitty icat 的 --place 不知道上方已有文字，只在图片独占预览区时用它播放动图
    if is_kitty && is_native && mime == "image/gif" && area.top == 0 {
        let mut cmd = Command::new("kitty");
        cmd.args(["icat", "--transfer-mode=file", "--image-id=10", &format!("--place={size}@0x0")]).arg(path);
        if let Ok(tty) = fs::OpenOptions::new().read(true).open("/dev/tty") { cmd.stdin(Stdio::from(tty)); }
        let _ = cmd.status();
        return;
    }
    // t=f 让终端按路径读文件：只有本机原生 kitty 才行，SSH 会话里路径指向的是远端
    let local = env::var_os("SSH_CONNECTION").is_none() && env::var_os("SSH_TTY").is_none();
    if is_kitty && is_native && local && mime == "image/png"
        && let Some(dim) = png_size(path)
        && kitty_png(path, dim, area)
    {
        return;
    }
    // JPEG/WebP 等 kitty 不能直接读：用 ffmpeg 转一次缩小的 PNG 缓存起来，之后同样走直通
    if is_kitty && is_native && local && matches!(mime.as_str(), "image/jpeg" | "image/webp" | "image/bmp" | "image/avif")
        && let Some(png) = to_png(path)
        && let Some(dim) = png_size(&png)
        && kitty_png(&png, dim, area)
    {
        return;
    }
    let mut cmd = Command::new("chafa");
    if is_kitty { cmd.args(["-f", "kitty"]); }
    let _ = cmd.args(["--animate=off", &format!("--size={size}")]).arg(path).status();
}

fn to_png(path: &Path) -> Option<PathBuf> {
    if !have("ffmpeg") { return None; }
    // 按内容哈希做键：源文件多半就在缓存目录里，命中时会被 touch，用 mtime 做键会每次都变
    let key = format!("conv-{:x}", xxhash_rust::xxh3::xxh3_128(&fs::read(path).ok()?));
    cached(&cache_dir(), &key, "png", |out| {
        // 预览用不着原图分辨率，长边压到 1280 让 PNG 编码和 kitty 解码都快
        quiet(Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(path)
            .args(["-frames:v", "1", "-vf", "scale='min(1280,iw)':'min(1280,ih)':force_original_aspect_ratio=decrease"]).arg(out))
    })
}

/// 图片：header 在上，图片占用剩下的区域
pub fn image(path: &Path, header: &[String], area: Area) {
    print_header(header, area);
    render_image(path, area.below(header.len()));
}

// ---------- 各类文件 ----------

pub fn video(path: &str, cache_dir: &Path, area: Area) {
    let key = file_key(path);
    let info = cached_text(cache_dir, &key, || {
        let p = ffprobe(path, "stream=codec_name,width,height:format=duration");
        if p.is_empty() { return None; }
        let mut parts = Vec::new();
        if let (Some(w), Some(h)) = (p.get("width"), p.get("height")) { parts.push(format!("{w}×{h}")); }
        if let Some(d) = p.get("duration").and_then(|d| d.parse::<f64>().ok()) { parts.push(human_duration(d)); }
        if let Some(c) = p.get("codec_name") { parts.push(c.clone()); }
        Some(parts.join(" · "))
    }).unwrap_or_default();
    let meta = [info, human_size(file_len(path))].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let header = [file_name(path), meta];

    // 取 10% 处的画面：第 0 帧经常是黑屏或片头
    let thumb = have("ffmpegthumbnailer").then(|| cached(cache_dir, &key, "png", |out| {
        quiet(Command::new("ffmpegthumbnailer").args(["-i", path, "-s", "1024", "-t", "10%", "-c", "png", "-o"]).arg(out))
    })).flatten();
    match thumb {
        Some(t) => image(&t, &header, area),
        None => { print_header(&header, area); println!("{GRAY}(no thumbnail){RESET}"); }
    }
}

fn pdf(path: &str, cache_dir: &Path, area: Area) {
    let key = file_key(path);
    let pages = have("pdfinfo").then(|| cached_text(cache_dir, &key, || {
        let out = Command::new("pdfinfo").arg(path).stderr(Stdio::null()).output().ok()?;
        String::from_utf8_lossy(&out.stdout).lines()
            .find_map(|l| l.strip_prefix("Pages:").map(|n| format!("{} pages", n.trim())))
    })).flatten();
    let meta = ["PDF".to_string(), pages.unwrap_or_default(), human_size(file_len(path))]
        .into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let header = [file_name(path), meta];

    if !have("pdftoppm") {
        print_header(&header, area);
        println!("{GRAY}(install poppler for page preview){RESET}");
        return;
    }
    let page = cached(cache_dir, &key, "png", |out| {
        // pdftoppm 会自己在前缀后面加 .png
        let prefix = out.with_extension("");
        quiet(Command::new("pdftoppm").args(["-png", "-f", "1", "-l", "1", "-singlefile", "-scale-to", "1024"]).arg(path).arg(&prefix))
    });
    match page {
        Some(p) => image(&p, &header, area),
        None => { print_header(&header, area); println!("{GRAY}(failed to render page){RESET}"); }
    }
}

fn audio(path: &str, cache_dir: &Path, area: Area) {
    let key = file_key(path);
    let info = cached_text(cache_dir, &key, || {
        let p = ffprobe(path, "format=duration:format_tags=title,artist,album");
        if p.is_empty() { return None; }
        let title = p.get("title").cloned().unwrap_or_default();
        let who = [p.get("artist"), p.get("album")].into_iter().flatten().cloned().collect::<Vec<_>>().join(" — ");
        let dur = p.get("duration").and_then(|d| d.parse::<f64>().ok()).map(human_duration).unwrap_or_default();
        Some(format!("{title}\n{who}\n{dur}"))
    }).unwrap_or_default();
    let mut it = info.splitn(3, '\n');
    let (title, who, dur) = (it.next().unwrap_or(""), it.next().unwrap_or(""), it.next().unwrap_or(""));
    let mut header = vec![if title.is_empty() { file_name(path) } else { title.to_string() }];
    if !who.is_empty() { header.push(who.to_string()); }
    header.push([dur.to_string(), human_size(file_len(path))].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "));

    // 内嵌封面（没有封面时 ffmpeg 会失败，cached 不会留下空文件）
    let cover = have("ffmpeg").then(|| cached(cache_dir, &key, "png", |out| {
        quiet(Command::new("ffmpeg").args(["-v", "error", "-y", "-i", path, "-an", "-frames:v", "1"]).arg(out))
    })).flatten();
    match cover {
        Some(c) => image(&c, &header, area),
        None => print_header(&header, area),
    }
}

fn directory(path: &str, area: Area) {
    let count = fs::read_dir(path).map(|d| d.count()).unwrap_or(0);
    print_header(&[format!("{}/", file_name(path)), format!("{count} items")], area);
    let max = area.rows.saturating_sub(2) as usize;
    if have("eza")
        && let Some((lines, _)) = head_of(Command::new("eza").args(["-1", "--color=always", "--icons=always", "--group-directories-first"]).arg(path), max)
    {
        for l in lines { println!("{l}"); }
        return;
    }
    let mut entries: Vec<(bool, String)> = fs::read_dir(path).into_iter().flatten().flatten()
        .map(|e| (!e.file_type().map(|t| t.is_dir()).unwrap_or(false), e.file_name().to_string_lossy().into_owned()))
        .collect();
    entries.sort();
    for (is_file, name) in entries.into_iter().take(max) {
        if is_file { println!("{name}"); } else { println!("\x1b[1;34m{name}/{RESET}"); }
    }
}

fn archive(path: &str, area: Area) {
    let max = area.rows.saturating_sub(3) as usize;
    let listing = if have("bsdtar") {
        head_of(Command::new("bsdtar").arg("-tf").arg(path), max)
    } else if have("unzip") && ext_of(path) == "zip" {
        head_of(Command::new("unzip").arg("-Z1").arg(path), max)
    } else {
        None
    };
    match listing {
        Some((lines, more)) => {
            print_header(&[file_name(path), format!("archive · {}", human_size(file_len(path)))], area);
            for l in lines { println!("{l}"); }
            if more { println!("{GRAY}…{RESET}"); }
        }
        // infer 把 sqlite 之类也归成 Archive，bsdtar 列不出来时按普通文件显示
        None => other_file(path, area),
    }
}

fn text_file(path: &str, area: Area) {
    print_header(&[file_name(path), human_size(file_len(path))], area);
    if have("bat") {
        let status = Command::new("bat")
            .args(["--color=always", "--paging=never", "--style=numbers", "--line-range=:500"])
            .arg(format!("--terminal-width={}", area.cols))
            .arg(path).stdin(Stdio::null()).stderr(Stdio::null()).status();
        if status.map(|s| s.success()).unwrap_or(false) { return; }
    }
    let mut buf = Vec::new();
    if let Ok(f) = fs::File::open(path) { let _ = f.take(TEXT_PREVIEW_LIMIT as u64).read_to_end(&mut buf); }
    print_text(&String::from_utf8_lossy(&buf));
}

fn other_file(path: &str, area: Area) {
    let desc = Command::new("file").arg("-b").arg(path).output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    print_header(&[file_name(path), human_size(file_len(path)), desc], area);
}

/// 单个本地文件：按类型分派
pub fn file(path: &str, cache_dir: &Path, area: Area) {
    let p = Path::new(path);
    if !p.exists() {
        println!("\x1b[31mFile not found{RESET}");
        println!("{GRAY}{path}{RESET}");
        return;
    }
    if p.is_dir() { return directory(path, area); }

    let kind = infer::get_from_path(p).ok().flatten();
    let mime = kind.map(|k| k.mime_type()).unwrap_or("");
    let ext = ext_of(path);
    if is_video_path(path) || mime.starts_with("video/") { return video(path, cache_dir, area); }
    if mime.starts_with("image/") || ext == "svg" { return image(p, &[], area); }
    if mime == "application/pdf" || ext == "pdf" { return pdf(path, cache_dir, area); }
    if mime.starts_with("audio/") || AUDIO_EXTS.contains(&ext.as_str()) { return audio(path, cache_dir, area); }
    if kind.map(|k| k.matcher_type() == infer::MatcherType::Archive).unwrap_or(false) { return archive(path, area); }

    let mut head = Vec::new();
    if let Ok(f) = fs::File::open(p) { let _ = f.take(64 * 1024).read_to_end(&mut head); }
    if !looks_binary(&head) { text_file(path, area) } else { other_file(path, area) }
}

/// 多个文件：上面列出文件清单，下面预览第一个
pub fn files(paths: &[String], cache_dir: &Path, area: Area) {
    const SHOW: usize = 5;
    println!("{BOLD}\x1b[36m{} files{RESET}", paths.len());
    for p in paths.iter().take(SHOW) {
        let mark = if Path::new(p).exists() { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
        println!("{mark} {}", fit_left(&short_path(p), area.cols.saturating_sub(2)));
    }
    let mut used = 1 + paths.len().min(SHOW);
    if paths.len() > SHOW { println!("{GRAY}… {} more{RESET}", paths.len() - SHOW); used += 1; }
    println!("{GRAY}{}{RESET}", "─".repeat(area.cols as usize));
    used += 1;
    if let Some(first) = paths.iter().find(|p| Path::new(p).exists()) {
        file(first, cache_dir, area.below(used));
    }
}

// ---------- 剪贴板文本 ----------

pub fn print_text(text: &str) {
    if text.len() <= TEXT_PREVIEW_LIMIT { println!("{}", text); return; }
    let mut cut = TEXT_PREVIEW_LIMIT;
    while !text.is_char_boundary(cut) { cut -= 1; }
    println!("{}\n\n{GRAY}… (only the first {} KiB shown, total {} KiB){RESET}", &text[..cut], cut / 1024, text.len() / 1024);
}

fn color_swatch(hex: &str, area: Area) {
    let h = hex.trim_start_matches('#');
    let h = if h.len() == 3 { h.chars().flat_map(|c| [c, c]).collect() } else { h.to_string() };
    let ch = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0);
    let (r, g, b) = (ch(0), ch(2), ch(4));
    let width = area.cols.min(40) as usize;
    for _ in 0..area.rows.saturating_sub(3).min(8) {
        println!("\x1b[48;2;{r};{g};{b}m{}{RESET}", " ".repeat(width));
    }
    println!("\n{BOLD}#{h}{RESET}  {GRAY}rgb({r}, {g}, {b}){RESET}");
}

// JSON 交给 jq 格式化 + 上色；失败（不是合法 JSON）就原样输出
fn pretty_json(text: &str) -> bool {
    let Ok(mut child) = Command::new("jq").args(["-C", "."]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return false };
    if let Some(mut stdin) = child.stdin.take() { let _ = stdin.write_all(text.as_bytes()); }
    match child.wait_with_output() {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => { print_text(&String::from_utf8_lossy(&out.stdout)); true }
        _ => false,
    }
}

pub fn clip_text(text: &str, area: Area) {
    let t = text.trim();
    if RE_HEX_COLOR.is_match(t) { return color_swatch(t, area); }
    if is_html(t) {
        print_header(&["HTML".to_string()], area);
        return print_text(&html_to_text(t));
    }
    let json_like = (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']'));
    if json_like && t.len() <= TEXT_PREVIEW_LIMIT * 4 && have("jq") && pretty_json(t) { return; }
    print_text(text);
}
