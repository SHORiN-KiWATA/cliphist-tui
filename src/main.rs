/*
 * cliphist-tui：基于 cliphist + fzf 的 Wayland 剪贴板 TUI。
 * - 列表：识别图片 / 视频 / 文件 / 网页图片 / 富文本 HTML / QQ / 微信等类型并打标签。
 * - 预览：图片 (kitty 直通 / chafa)、视频、PDF、音频、文本文件、目录、压缩包、JSON、颜色值，见 preview.rs。
 * - 自动刷新：wl-paste --watch 在剪贴板变化时调用本程序的 watch 子命令，经 Unix socket 通知 fzf reload。
 * - 实测 (2026-10，kitty，约 240 条历史)：空闲约 13 MiB (其中 fzf 约 9.5 MiB)、CPU 为 0；
 *   打开到第一屏 8ms，图片预览 3–5ms (有缓存)，复制约 7ms，列表刷新 3–4ms。
 */

mod preview;

use clap::{Parser, Subcommand};
use preview::Area;
use crossterm::terminal::size;
use regex::Regex;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

/// cliphist-tui - Wayland 剪贴板 TUI (cliphist + fzf)
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Run,
    List,
    Preview { id: String },
    Copy { id: String },
    Open { id: String },
    Delete { id: String },
    DeleteAll,
    /// 由 `wl-paste --watch` 在剪贴板变化时调用（内部使用）
    #[command(hide = true)]
    Watch,
}

const IMG_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"];
const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "flv", "wmv", "ts"];
// 文本预览上限：几 MB 的内容整段喂给 fzf 预览窗只会卡
const TEXT_PREVIEW_LIMIT: usize = 256 * 1024;

static RE_IMG_SRC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?i)<img\b[^>]*?\ssrc="([^"]+)"#).unwrap());
static RE_QQ_FILEPATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"filepath="([^"]+)""#).unwrap());

// 取最后一段路径的小写扩展名（忽略 URL 的 ?query / #fragment）
fn ext_of(s: &str) -> String {
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let name = s.rsplit('/').next().unwrap_or(s);
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

fn is_image_ext(s: &str) -> bool { IMG_EXTS.contains(&ext_of(s).as_str()) }

// .ts 同时是 TypeScript 后缀：文件在本地时额外校验 MPEG-TS 同步字节 (0x47 每 188 字节一个)
fn is_video_path(p: &str) -> bool {
    let ext = ext_of(p);
    if !VIDEO_EXTS.contains(&ext.as_str()) { return false; }
    if ext != "ts" { return true; }
    let mut buf = [0u8; 189];
    match fs::File::open(p).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf[0] == 0x47 && buf[188] == 0x47,
        Err(_) => false,
    }
}

fn expand_tilde(p: &str) -> String {
    match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}

// file:// URI -> 本地路径（兼容 file://localhost/ 与 QQ 的 file:////home/...）
fn uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = urlencoding::decode(rest).map(|s| s.into_owned()).unwrap_or_else(|_| rest.to_string());
    Some(format!("/{}", decoded.trim_start_matches('/')))
}

static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*(>|$)").unwrap());

// 只认「整段 HTML 除了标签就只有一张图」的片段（浏览器/QQ 复制图片）。
// 带正文的富文本不算，否则复制时只剩一张图、正文全丢。
fn html_img_src(text: &str) -> Option<String> {
    if !text.trim_start().starts_with('<') { return None; }
    let src = RE_IMG_SRC.captures(text).map(|caps| caps[1].replace("&amp;", "&"))?;
    let rest = RE_TAG.replace_all(text, "");
    let rest = rest.replace("&nbsp;", "").replace('…', "");
    rest.trim().is_empty().then_some(src)
}

// 剪贴板里的富文本 HTML（浏览器以外的一些应用只提供 text/html）
static RE_HTML_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(
    r"(?i)^\s*<(!doctype|html|meta|head|body|div|span|p|a|b|i|u|img|table|ul|ol|li|h[1-6]|br|strong|em|font|section|article|pre|code|blockquote)\b"
).unwrap());
static RE_HTML_DROP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<script\b.*?</script>|<style\b.*?</style>|<head\b.*?</head>").unwrap());
static RE_HTML_BREAK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<br\s*/?>|</(p|div|li|tr|h[1-6]|blockquote|pre|table|ul|ol)>").unwrap());

fn is_html(text: &str) -> bool { RE_HTML_START.is_match(text) }

// HTML -> 纯文本：块级标签换行，去掉其余标签，还原常见实体
fn html_to_text(html: &str) -> String {
    let s = RE_HTML_DROP.replace_all(html, "");
    let s = RE_HTML_BREAK.replace_all(&s, "\n");
    let s = RE_TAG.replace_all(&s, "");
    let s = s.replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">")
        .replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&");
    let mut out = String::new();
    let mut blank = false;
    for line in s.lines().map(str::trim) {
        if line.is_empty() {
            if !blank && !out.is_empty() { out.push('\n'); }
            blank = true;
        } else {
            out.push_str(line);
            out.push('\n');
            blank = false;
        }
    }
    out.trim_end().to_string()
}

fn url_host(url: &str) -> &str {
    url.split("://").nth(1).unwrap_or("").split(['/', '?', '#']).next().unwrap_or("")
}

// 部分图床的 src 是缩略图，复制/打开时换成原图（B 站：去掉 @672w_378h_... 处理后缀）
fn original_image_url(url: &str) -> String {
    if url_host(url).ends_with("hdslb.com") {
        if let Some((base, _)) = url.split_once('@') { return base.to_string(); }
    }
    url.to_string()
}

// 二进制判定：含 NUL，或不是合法 UTF-8 且乱码比例明显（只看开头 64KB）
fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(64 * 1024)];
    if head.contains(&0) { return true; }
    match std::str::from_utf8(head) {
        Ok(_) => false,
        // 截断处可能切在多字节字符中间，error_len() == None 说明只是末尾不完整
        Err(e) if e.error_len().is_none() => false,
        Err(_) => {
            let lossy = String::from_utf8_lossy(head);
            lossy.matches('\u{FFFD}').count() * 20 > lossy.chars().count()
        }
    }
}

// 进程内查 PATH，省掉每次 spawn `which`
fn have(cmd: &str) -> bool {
    env::var_os("PATH").map(|p| env::split_paths(&p).any(|d| d.join(cmd).is_file())).unwrap_or(false)
}

// 修复 URL 编码，避免把 `/` 转成 `%2F`
fn encode_path(path: &str) -> String {
    urlencoding::encode(path).replace("%2F", "/")
}

// 高鲁棒性的 MIME 获取（结合 infer 的速度与 file 命令的准确性）
fn get_mime(bytes: &[u8]) -> String {
    if let Some(kind) = infer::get(bytes) {
        return kind.mime_type().to_string();
    }
    // Fallback: file 命令，准确检测 text/html 等纯文本
    if let Ok(mut child) = Command::new("file")
        .args(["-b", "--mime-type", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn() 
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(bytes);
        }
        if let Ok(output) = child.wait_with_output() {
            let mime = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !mime.is_empty() {
                return mime;
            }
        }
    }
    "".to_string()
}

fn get_mime_from_path(p: &Path) -> String {
    if let Ok(Some(kind)) = infer::get_from_path(p) {
        return kind.mime_type().to_string();
    }
    if let Ok(output) = Command::new("file").args(["-b", "--mime-type"]).arg(p).output() {
        let mime = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !mime.is_empty() { return mime; }
    }
    "".to_string()
}

// cliphist delete 只按每行开头的 id 删除，不需要先跑一遍完整的 cliphist list（大库要几百 ms）
fn delete_clip_by_id(id: &str) {
    if let Ok(mut c) = Command::new("cliphist").arg("delete").stdin(Stdio::piped()).stderr(Stdio::null()).spawn() {
        if let Some(mut stdin) = c.stdin.take() {
            let _ = writeln!(stdin, "{}\t", id.trim());
        }
        let _ = c.wait();
    }
}

// 只读第一行就杀掉 cliphist，避免为了一个 id 等完整个列表
fn first_clip_id() -> String {
    let Ok(mut child) = Command::new("cliphist")
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return String::new();
    };

    let mut line = String::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = BufReader::new(stdout).read_line(&mut line);
    }
    let _ = child.kill();
    let _ = child.wait();

    line.split_once('\t')
        .map(|(id, _)| id)
        .unwrap_or(&line)
        .trim()
        .to_string()
}

fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(env::temp_dir).join("cliphist-tui")
}

fn main() {
    let cli = Cli::parse();
    let cache_dir = cache_dir();
    let _ = fs::create_dir_all(&cache_dir);

    let term = env::var("TERM").unwrap_or_default();
    let wez = env::var("WEZTERM_EXECUTABLE").unwrap_or_else(|_| "0".to_string());
    let kitty_cache_path = format!("/dev/shm/shorinclip_kitty_{}_{}", term, wez);
    
    let mut enable_icat = "0".to_string();
    let mut is_native = "0".to_string();

    if let Ok(content) = fs::read_to_string(&kitty_cache_path) {
        if content.contains("ENABLE_ICAT=1") { enable_icat = "1".to_string(); }
        if content.contains("IS_NATIVE_KITTY=1") { is_native = "1".to_string(); }
    } else {
        if term == "xterm-kitty" {
            enable_icat = "1".to_string();
            is_native = "1".to_string();
        } else if wez != "0" && !wez.is_empty() {
            enable_icat = "0".to_string();
            is_native = "0".to_string();
        }
        let cache_data = format!("export ENABLE_ICAT={}; export IS_NATIVE_KITTY={}", enable_icat, is_native);
        let _ = fs::write(&kitty_cache_path, cache_data);
    }

    unsafe {
        env::set_var("ENABLE_ICAT", enable_icat);
        env::set_var("IS_NATIVE_KITTY", is_native);
    }

    match &cli.command {
        Some(Commands::Run) => run_tui(&cache_dir),
        Some(Commands::List) => {
            let mut stdout = std::io::stdout();
            stream_formatted_list(&mut stdout);
        }
        Some(Commands::Preview { id }) => run_preview(id, &cache_dir),
        Some(Commands::Copy { id }) => run_copy(id, &cache_dir),
        Some(Commands::Open { id }) => run_open(id, &cache_dir),
        Some(Commands::Delete { id }) => delete_clip_by_id(id),
        Some(Commands::Watch) => run_watch(),
        Some(Commands::DeleteAll) => {
            // 交给 cliphist 自己清空：会遵循用户在 cliphist 配置里自定义的 db 路径
            let _ = Command::new("cliphist").arg("wipe").stdin(Stdio::null()).status();
            if env::var("ENABLE_ICAT").unwrap_or_default() == "1" {
                if let Ok(mut tty) = fs::OpenOptions::new().write(true).open("/dev/tty") {
                    let _ = tty.write_all(b"\x1B_Ga=d,d=A\x1B\\");
                }
            }
        }
        None => run_tui(&cache_dir),
    }
}

fn get_decode(id: &str) -> Vec<u8> {
    let mut decode_cmd = Command::new("cliphist")
        .arg("decode")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Failed to spawn cliphist");
    
    if let Some(mut stdin) = decode_cmd.stdin.take() {
        let _ = writeln!(stdin, "{}\t", id.trim());
    }

    let mut output = Vec::new();
    decode_cmd.stdout.take().unwrap().read_to_end(&mut output).unwrap();
    let _ = decode_cmd.wait();
    output
}

#[derive(Clone, Copy, PartialEq)]
enum RowKind { WebImgHtml, BinImage, Other }

fn row_kind(c: &str) -> RowKind {
    let c = c.trim();
    if let Some(info) = c.strip_prefix("[[ binary data ").and_then(|s| s.strip_suffix(" ]]")) {
        let fmt = info.split_whitespace().nth(2).unwrap_or("");
        return if IMG_EXTS.contains(&fmt) { RowKind::BinImage } else { RowKind::Other };
    }
    match html_img_src(c) {
        Some(src) if src.starts_with("http://") || src.starts_with("https://") => RowKind::WebImgHtml,
        _ => RowKind::Other,
    }
}

struct Row { id: String, num_id: u64, content: String, kind: RowKind }

// 边读 cliphist list 边格式化边输出，不把整个列表攒在内存里
fn stream_formatted_list<W: Write>(mut writer: W) {
    if let Ok(mut child) = Command::new("cliphist")
        // 默认只给 100 字符预览，长路径/HTML 会被截断导致扩展名、img src 都认不出来
        .args(["-preview-width", "512", "list"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut num = 1;
            // 晚一行输出：判断一条是否该藏起来，需要看它上下相邻的两条
            let mut above: Option<(u64, RowKind)> = None;
            let mut pending: Option<Row> = None;

            let mut emit = |row: Row, above: Option<(u64, RowKind)>, below: Option<(u64, RowKind)>, writer: &mut W| {
                // 同时跑了 `wl-paste --type image --watch` 时，浏览器复制一张图会先后存下 HTML 和图片本身，
                // id 紧挨着。图片那条已经能用，HTML 那条只是重复，藏起来
                let next_to_image = |n: Option<(u64, RowKind)>| n.is_some_and(|(id, k)| k == RowKind::BinImage && id.abs_diff(row.num_id) == 1);
                if row.kind == RowKind::WebImgHtml && (next_to_image(above) || next_to_image(below)) { return; }
                let formatted = format_content(&row.content);
                let _ = writeln!(writer, "{}\t\x1b[90m{:<2} \x1b[0m{}", row.id, num, formatted);
                num += 1;
                // 首屏先冲出去：cliphist list 解析大库要几百 ms，不能等 64KB 缓冲攒满才让 fzf 看到第一行
                if num == 64 { let _ = writer.flush(); }
            };

            for line in reader.lines().map_while(Result::ok) {
                if line.contains("<html") && line.contains("[表情]") { continue; }
                let Some((id, content)) = line.split_once('\t') else { continue };
                let row = Row { id: id.to_string(), num_id: id.trim().parse().unwrap_or(0), content: content.to_string(), kind: row_kind(content) };
                let cur = (row.num_id, row.kind);
                if let Some(prev) = pending.take() {
                    let prev_key = (prev.num_id, prev.kind);
                    emit(prev, above, Some(cur), &mut writer);
                    above = Some(prev_key);
                }
                pending = Some(row);
            }
            if let Some(prev) = pending.take() { emit(prev, above, None, &mut writer); }
        }
        let _ = child.wait();
    }
}

// cliphist list 把换行压成了空格：按 " /"、" ~/"、" file://" 边界把多文件列表切回去
fn split_list_paths(c: &str) -> Vec<&str> {
    let b = c.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..b.len() {
        if b[i - 1] == b' ' && (c[i..].starts_with('/') || c[i..].starts_with("~/") || c[i..].starts_with("file://")) {
            out.push(c[start..i - 1].trim());
            start = i;
        }
    }
    out.push(c[start..].trim());
    out
}

fn format_content(c: &str) -> String {
    let t = "\x1b[1;35m"; let p = "\x1b[1;34m"; let cy = "\x1b[1;36m"; let g = "\x1b[90m"; let r = "\x1b[0m";
    let c = c.trim();

    // [[ binary data 781 KiB png 875x1015 ]]
    if let Some(info) = c.strip_prefix("[[ binary data ").and_then(|s| s.strip_suffix(" ]]")) {
        let fields: Vec<&str> = info.split_whitespace().collect();
        if let [num, unit, fmt, dim] = fields[..] {
            if IMG_EXTS.contains(&fmt) {
                return format!("{t}[IMG]Bin.{fmt}{r} {g}{dim} {num} {unit}{r}");
            }
        }
        return format!("{cy}[BINARY]{r} {g}{info}{r}");
    }
    // cliphist 没识别成二进制、但其实是乱码的内容
    if c.contains('\u{FFFD}') && c.matches('\u{FFFD}').count() * 20 > c.chars().count() {
        return format!("{cy}[BINARY]{r}");
    }

    if c.contains("QQRichEditFormat") && c.contains("EditElement type=\"7\"") { return format!("{t}[VIDEO_HTML]QQ{r}"); }
    if let Some(src) = html_img_src(c) {
        if src.starts_with("file://") {
            let who = if src.contains("/QQ/") || src.contains("qq") { "QQ" } else { "File" };
            return format!("{p}[IMG_HTML]{who}{r}");
        }
        if src.starts_with("http://") || src.starts_with("https://") {
            return format!("{p}[IMG_HTML]Web{r} {g}{}{r}", url_host(&src));
        }
    }
    if is_html(c) {
        let body = html_to_text(c).split_whitespace().collect::<Vec<_>>().join(" ");
        return format!("{cy}[HTML]{r} {body}");
    }

    if c.starts_with("file://") || c.starts_with('/') || c.starts_with("~/") {
        let items = split_list_paths(c);
        let names = items.iter()
            .map(|s| s.trim_end_matches('/').rsplit('/').next().unwrap_or(s))
            .map(|s| urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        if items.len() > 1 { return format!("{cy}[FILES]{}{r} {g}{names}{r}", items.len()); }

        let is_uri = c.starts_with("file://");
        if is_uri && c.contains(' ') { return c.to_string(); }
        let path = if is_uri { uri_to_path(c).unwrap_or_default() } else { expand_tilde(c) };
        let src = if is_uri { "Url" } else { "Path" };
        let ext = ext_of(&path);
        let label = if is_video_path(&path) {
            format!("{t}[VIDEO]{src}.{ext}{r}")
        } else if path.contains("/.config/QQ/") && is_image_ext(&path) {
            format!("{p}[IMG_URL]QQ{r}")
        } else if is_uri && path.contains("xwechat") && path.contains("temp") {
            format!("{p}[IMG_URL]WeChat{r}")
        } else if ext == "gif" {
            format!("{p}[IMG]{src}.gif{r}")
        } else if is_image_ext(&path) {
            format!("{t}[IMG]{src}.{ext}{r}")
        } else if is_uri {
            format!("{cy}[URL]File{r}")
        } else {
            // 普通路径文本（比如 /etc/hosts）照常当文本显示
            return c.to_string();
        };
        return format!("{label} {g}{names}{r}");
    }
    c.to_string()
}

/// 完整内容的分类：预览 / 复制 / 打开共用，避免三处 if 链各自判断、条件对不上
enum Clip<'a> {
    Image(String),          // 二进制图片，带 mime
    QQVideo(String),        // QQ 富文本视频，本地 filepath
    HtmlImg(String),        // 浏览器/QQ 复制图片时只存下来的 <img src="...">
    Files(Vec<String>),     // text/uri-list 或换行分隔的本地路径
    Binary(String),         // 非图片二进制，带 mime
    Text(&'a str),
}

fn classify<'a>(raw: &[u8], text: &'a str) -> Clip<'a> {
    if let Some(kind) = infer::get(raw) {
        if kind.mime_type().starts_with("image/") { return Clip::Image(kind.mime_type().to_string()); }
    }
    if looks_binary(raw) { return Clip::Binary(get_mime(raw)); }

    if text.contains("QQRichEditFormat") && text.contains("EditElement type=\"7\"") {
        if let Some(caps) = RE_QQ_FILEPATH.captures(text) { return Clip::QQVideo(caps[1].to_string()); }
    }
    if let Some(src) = html_img_src(text) { return Clip::HtmlImg(src); }

    let mut files = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if line.starts_with("file://") {
            // URI 里不会有裸空白，有的话就是一段以 file:// 开头的普通文字
            if line.contains(char::is_whitespace) { return Clip::Text(text); }
            match uri_to_path(line) { Some(p) => files.push(p), None => return Clip::Text(text) }
        } else if line.starts_with('/') || line.starts_with("~/") {
            // 纯路径必须真实存在才算文件，否则就是普通文本
            let p = expand_tilde(line);
            if !Path::new(&p).exists() { return Clip::Text(text); }
            files.push(p);
        } else {
            return Clip::Text(text);
        }
    }
    if files.is_empty() { Clip::Text(text) } else { Clip::Files(files) }
}

fn uri_list(paths: &[String]) -> String {
    paths.iter().map(|p| format!("file://{}", encode_path(p))).collect::<Vec<_>>().join("\r\n")
}

// 先写临时文件再改名：预览进程随时会被 fzf 杀掉，直接写最终文件名会留下截断的缓存
fn write_atomic(path: &Path, data: &[u8]) -> bool {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    if fs::write(&part, data).is_ok() && fs::rename(&part, path).is_ok() { return true; }
    let _ = fs::remove_file(&part);
    false
}

// 缓存命中时刷新 mtime，让 mtime 代表「最后一次使用」（atime 会被备份/索引工具刷掉，不可靠）
fn touch(path: &Path) {
    if let Ok(f) = fs::File::options().write(true).open(path) { let _ = f.set_modified(std::time::SystemTime::now()); }
}

// 把图片字节落到缓存目录，按内容哈希命名
fn cache_image(raw: &[u8], mime: &str, cache_dir: &Path) -> PathBuf {
    let ext = mime.split('/').last().unwrap_or("png");
    let file = cache_dir.join(format!("{:x}.{}", xxhash_rust::xxh3::xxh3_128(raw), ext));
    if file.exists() { touch(&file); } else { write_atomic(&file, raw); }
    file
}

// 下载网页图片到缓存（按 URL 哈希命名，扩展名按实际内容定），失败或不是图片返回 None
fn fetch_web_image(url: &str, cache_dir: &Path) -> Option<PathBuf> {
    let stem = format!("web-{:x}", xxhash_rust::xxh3::xxh3_128(url.as_bytes()));
    for ext in IMG_EXTS {
        let f = cache_dir.join(format!("{stem}.{ext}"));
        if f.exists() { touch(&f); return Some(f); }
    }
    let part = cache_dir.join(format!("{stem}.part"));
    let ok = Command::new("curl")
        .args(["-sfL", "--connect-timeout", "3", "--max-time", "10", "--max-filesize", "50M", "-A", "Mozilla/5.0", "-o"]).arg(&part).arg(url)
        .stdin(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);
    let kind = ok.then(|| infer::get_from_path(&part).ok().flatten()).flatten();
    match kind.filter(|k| k.mime_type().starts_with("image/")) {
        Some(k) => {
            let ext = match k.extension() { "jpeg" => "jpg", e => e };
            let f = cache_dir.join(format!("{stem}.{ext}"));
            fs::rename(&part, &f).ok()?;
            Some(f)
        }
        None => { let _ = fs::remove_file(&part); None }
    }
}

// 预览缓存多少天没被用过就清掉（都能按需重新生成）
const CACHE_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

// 每个实例的运行时文件（fzf 的 socket、上次看到的剪贴板 id）放在 $XDG_RUNTIME_DIR 下：
// 目录只有本用户可访问，浏览器等也连不到 Unix socket
fn runtime_dir() -> PathBuf {
    let base = dirs::runtime_dir().unwrap_or_else(env::temp_dir);
    let dir = base.join("cliphist-tui");
    let _ = fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    dir
}

// 窗口被直接关掉时进程收到 SIGHUP，来不及清理自己的运行时文件；启动时把属于已退出进程的残留清掉
fn prune_runtime(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let pid = name.split('.').next().unwrap_or("");
            if !pid.is_empty() && !Path::new("/proc").join(pid).exists() { let _ = fs::remove_file(e.path()); }
        }
    }
    // 旧版本留在 /dev/shm 的文件
    if let Ok(entries) = fs::read_dir("/dev/shm") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("shorinclip_last_id_") || name.starts_with("cliphist-tui_last_id_") { let _ = fs::remove_file(e.path()); }
        }
    }
}

// 给 sh -c 用的单引号转义（fzf 的 bind/preview 都经过 shell）
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// 按 mtime（命中时会 touch）清理预览缓存，顺带清掉半途失败的 .part 和 0 字节文件
fn prune_cache(cache_dir: &Path) {
    let Ok(entries) = fs::read_dir(cache_dir) else { return };
    let now = std::time::SystemTime::now();
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() { continue; }
        let age = meta.modified().ok().and_then(|t| now.duration_since(t).ok()).unwrap_or_default();
        let name = e.file_name();
        let part = name.to_string_lossy().contains(".part");
        if meta.len() == 0 || age > CACHE_MAX_AGE || (part && age > Duration::from_secs(3600)) {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn run_tui(cache_dir: &Path) {
    let mut wait_timeout = 50;
    while wait_timeout > 0 {
        if let Ok((cols, lines)) = size() { if cols >= 35 && lines >= 25 { break; } }
        thread::sleep(Duration::from_millis(50));
        wait_timeout -= 1;
    }

    let exe = shell_quote(&env::current_exe().unwrap().to_string_lossy());
    let rt = runtime_dir();
    prune_runtime(&rt);
    let cache_dir_owned = cache_dir.to_path_buf();
    thread::spawn(move || prune_cache(&cache_dir_owned));

    // fzf 的控制接口走 Unix socket，不再监听 localhost 随机端口：
    // TCP 端口会接受本机任何来源（包括网页里的 JS）发来的 reload/execute，等于开了个本地命令执行口子；
    // 端口随机还可能撞上已占用的端口导致 fzf 直接起不来
    let pid = std::process::id();
    let sock = rt.join(format!("{pid}.sock"));

    // 【解决闪烁】：在启动 watcher 之前初始化 LAST_ID 文件，
    // 否则 wl-paste --watch 启动时会立刻跑一次，发现文件不存在就触发 reload，图片刚绘制就被清掉重绘
    let last_id = rt.join(format!("{pid}.last_id"));
    let _ = fs::write(&last_id, first_clip_id());

    // 剪贴板每变化一次，wl-paste 就调用一次本程序的 watch 子命令（参数经环境变量传入）
    let mut watcher = Command::new("wl-paste")
        .arg("--watch").arg(env::current_exe().unwrap()).arg("watch")
        .env("CT_LAST_ID", &last_id)
        .env("CT_SOCK", &sock)
        .env("CT_RELOAD", format!("reload({exe} list)"))
        .spawn().unwrap();

    let mut fzf = Command::new("fzf")
        // 限制 fzf 的 Go 调度器最多用 2 个系统线程（列表很小，用不上更多并行）
        .env("GOMAXPROCS", "2")
        .arg("--ansi").arg(format!("--listen={}", sock.to_string_lossy()))
        .arg(format!("--bind=ctrl-r:reload({exe} list)"))
        .arg(format!("--bind=ctrl-x:execute-silent({exe} delete {{1}})+reload({exe} list)"))
        .arg(format!("--bind=alt-x:execute-silent({exe} delete-all)+reload({exe} list)"))
        .arg(format!("--bind=ctrl-o:execute-silent({exe} open {{1}})"))
        .arg(format!("--bind=ctrl-e:execute-silent({exe} open {{1}})"))
        .arg("--prompt=󰅍 > ")
        .arg("--bind=ctrl-/:toggle-preview")
        .arg("--bind=alt-j:preview-down,alt-k:preview-up")
        .arg("--header=C^-X: Delete | Alt+X: D-All | C^-R: Reload | C^-O/E: Open | Enter/C^-F: Paste | C^-/: Preview | Alt+J/K: Scroll")
        .arg("--color=header:italic:yellow,prompt:blue,pointer:blue")
        .arg("--info=hidden").arg("--no-sort").arg("--layout=reverse")
        .arg("--with-nth=2..").arg("--delimiter=\t")
        .arg("--preview-window=down:60%,wrap")
        .arg(format!("--preview={exe} preview {{1}}"))
        .arg(format!("--bind=enter:execute-silent({exe} copy {{1}})+accept"))
        .arg(format!("--bind=ctrl-f:execute-silent({exe} copy {{1}})+accept"))
        .arg(format!("--bind=ctrl-l:execute-silent({exe} copy {{1}})+accept"))
        .arg(format!("--bind=ctrl-h:execute-silent({exe} copy {{1}})+accept"))
        .stdin(Stdio::piped()).spawn().unwrap();

    if let Some(stdin) = fzf.stdin.take() {
        // 大块缓冲减少写入次数；首屏会提前 flush，见 stream_formatted_list
        let mut writer = BufWriter::with_capacity(64 * 1024, stdin);
        stream_formatted_list(&mut writer);
        let _ = writer.flush();
    }

    fzf.wait().unwrap();
    let _ = watcher.kill();
    let _ = fs::remove_file(&last_id); // 退出后清理运行时文件
    let _ = fs::remove_file(&sock);
}

// 通过 Unix socket 给 fzf 的 --listen 接口发一个动作（手写最小 HTTP 请求，不再依赖 curl）
fn post_fzf_action(sock: &Path, action: &str) -> std::io::Result<()> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    write!(stream, "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{action}", action.len())?;
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    Ok(())
}

// wl-paste --watch 的回调：等 cliphist 把新内容存进去（守护进程是并行跑的，可能稍晚），
// 发现最新 id 变了就让 fzf reload。新内容没被存（比如密码管理器标记的敏感内容）就等满 2 秒放弃
fn run_watch() {
    // 剪贴板内容从 stdin 灌进来，用不到，立刻关掉，免得源程序写大图时卡在管道上
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        drop(unsafe { fs::File::from_raw_fd(0) });
    }
    let (Some(last_id), Some(sock), Ok(action)) = (env::var_os("CT_LAST_ID"), env::var_os("CT_SOCK"), env::var("CT_RELOAD")) else { return };
    for _ in 0..10 {
        let current = first_clip_id();
        let last = fs::read_to_string(&last_id).unwrap_or_default();
        if !current.is_empty() && current != last.trim() {
            let _ = fs::write(&last_id, &current);
            let _ = post_fzf_action(Path::new(&sock), &action);
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn run_preview(id: &str, cache_dir: &Path) {
    if env::var("ENABLE_ICAT").unwrap_or_default() == "1" {
        if let Ok(mut tty) = fs::OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(b"\x1B_Ga=d,d=A\x1B\\");
        }
    }

    let raw_bytes = get_decode(id);
    if raw_bytes.is_empty() { return; }

    let text_cow = String::from_utf8_lossy(&raw_bytes);
    let decoded_text = text_cow.trim_end_matches(|c| c == '\n' || c == '\r');

    let area = Area::from_env();
    match classify(&raw_bytes, decoded_text) {
        Clip::Image(mime) => preview::image(&cache_image(&raw_bytes, &mime, cache_dir), &[], area),
        Clip::QQVideo(path) => {
            if Path::new(&path).exists() { preview::video(&path, cache_dir, area); } else { preview::print_text(decoded_text); }
        }
        Clip::HtmlImg(src) => {
            let local = uri_to_path(&src).filter(|p| Path::new(p).exists()).map(PathBuf::from);
            let web = || (src.starts_with("http://") || src.starts_with("https://"))
                .then(|| fetch_web_image(&src, cache_dir)).flatten();
            match local.or_else(web) {
                Some(img) => preview::image(&img, &[], area),
                None => preview::print_text(decoded_text),
            }
        }
        Clip::Files(paths) if paths.len() == 1 => preview::file(&paths[0], cache_dir, area),
        Clip::Files(paths) => preview::files(&paths, cache_dir, area),
        Clip::Binary(mime) => {
            let mime = if mime.is_empty() { "unknown".to_string() } else { mime };
            println!("\x1b[1;36m[binary data]\x1b[0m {} bytes, {}", raw_bytes.len(), mime);
        }
        Clip::Text(text) => preview::clip_text(text, area),
    }
}

fn notify(title: &str, body: &str) {
    Command::new("notify-send").args([title, body]).spawn().ok();
}

fn run_copy(id: &str, cache_dir: &Path) {
    let raw_bytes = get_decode(id);
    let text_cow = String::from_utf8_lossy(&raw_bytes);
    let text = text_cow.trim_end_matches(|c| c == '\n' || c == '\r');

    match classify(&raw_bytes, text) {
        Clip::Image(mime) => {
            wl_copy(&raw_bytes, Some(&mime));
            notify("Copied Image", id);
        }
        Clip::QQVideo(path) => {
            wl_copy(uri_list(&[path.clone()]).as_bytes(), Some("text/uri-list"));
            notify("Copied QQ Video", &path);
        }
        Clip::HtmlImg(src) if src.starts_with("file://") => {
            let path = uri_to_path(&src).unwrap_or_default();
            wl_copy(uri_list(&[path.clone()]).as_bytes(), Some("text/uri-list"));
            notify("Copied QQ Link", &path);
        }
        Clip::HtmlImg(src) => {
            // 网页图片：下载原图，以真正的图片格式放回剪贴板，可以直接粘贴进聊天软件
            let url = original_image_url(&src);
            match fetch_web_image(&url, cache_dir).and_then(|f| fs::read(&f).ok().map(|b| (f, b))) {
                Some((f, bytes)) => {
                    let mime = get_mime_from_path(&f);
                    wl_copy(&bytes, Some(&mime));
                    notify("Copied Web Image", &url);
                }
                None => {
                    wl_copy(&raw_bytes, Some("text/html"));
                    notify("Image download failed", "Copied as HTML instead");
                }
            }
        }
        Clip::Files(paths) => {
            wl_copy(uri_list(&paths).as_bytes(), Some("text/uri-list"));
            let mut body = paths.iter().take(5).cloned().collect::<Vec<_>>().join("\n");
            if paths.len() > 5 { body.push_str(&format!("\n… {} more", paths.len() - 5)); }
            notify(if paths.len() > 1 { "Copied Files" } else { "Copied File Link" }, &body);
        }
        Clip::Binary(_) | Clip::Text(_) => wl_copy(&raw_bytes, None),
    }

    delete_clip_by_id(id);
}

fn run_open(id: &str, cache_dir: &Path) {
    let raw_bytes = get_decode(id);
    let text_cow = String::from_utf8_lossy(&raw_bytes);
    let text = text_cow.trim_end_matches(|c| c == '\n' || c == '\r');

    match classify(&raw_bytes, text) {
        Clip::Image(mime) => xdg_open(&cache_image(&raw_bytes, &mime, cache_dir).to_string_lossy()),
        Clip::QQVideo(path) => smart_open(&path),
        Clip::HtmlImg(src) if src.starts_with("file://") => {
            if let Some(path) = uri_to_path(&src) { xdg_open(&path); }
        }
        Clip::HtmlImg(src) => {
            let url = original_image_url(&src);
            match fetch_web_image(&url, cache_dir) {
                Some(f) => xdg_open(&f.to_string_lossy()),
                None => xdg_open(&url),
            }
        }
        Clip::Files(paths) => {
            // 多个文件只打开第一个，避免一下子弹出一堆窗口
            let path = &paths[0];
            if Path::new(path).exists() { smart_open(path); }
            else { notify("Open Error", &format!("File missing: {}", path)); }
        }
        Clip::Text(t) if (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace) => xdg_open(t),
        Clip::Binary(_) | Clip::Text(_) => {}
    }
}

fn wl_copy(data: &[u8], mime: Option<&str>) {
    let mut cmd = Command::new("wl-copy");
    if let Some(m) = mime { cmd.arg("--type").arg(m); }
    if let Ok(mut c) = cmd.stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = c.stdin.take() {
            let _ = stdin.write_all(data);
        }
        let _ = c.wait();
    }
}

fn xdg_open(target: &str) {
    Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}

fn smart_open(path: &str) {
    if is_video_path(path) && have("mpv") {
        Command::new("mpv")
            .arg("--wayland-app-id=floating-mpv")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    } else {
        xdg_open(path);
    }
}
