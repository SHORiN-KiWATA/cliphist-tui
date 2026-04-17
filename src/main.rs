/*
 * 功能总结：
 * 1. 作为剪贴板管理器 (cliphist) 的高级 TUI 交互界面。
 * 2. 对剪贴板记录进行智能格式化显示，支持极其丰富的特殊格式提取 (微信/QQ/图片/视频等)。
 * 3. 结合 fzf 提供高亮检索，通过 HTTP 监听提供平滑无闪烁的热重载。
 * 4. 内置高度优化的极速预览机制，支持 kitty icat / chafa / mpv 视频缩略图。
 * 5. 极致性能与资源优化：采用零拷贝正则平替、管道流式渲染、I/O 批处理缓冲，并通过环境变量压制 Go 运行时，实现最低 CPU 与内存占用。
 */

use clap::{Parser, Subcommand};
use crossterm::terminal::size;
use regex::Regex;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// cliphist-tui - 高级剪贴板 TUI 工具 (Rust 极速重构版)
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
}

fn is_video_ext(s: &str) -> bool {
    let lower = s.to_lowercase();
    lower.ends_with(".mp4") || lower.ends_with(".mkv") || lower.ends_with(".webm") ||
    lower.ends_with(".avi") || lower.ends_with(".mov") || lower.ends_with(".flv") || 
    lower.ends_with(".wmv") || lower.ends_with(".ts")
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

// 完美删除逻辑：通过 ID 获取完整内容行后再删除
fn delete_clip_by_id(id: &str) {
    if let Ok(list) = Command::new("cliphist").arg("list").output() {
        let list_str = String::from_utf8_lossy(&list.stdout);
        let prefix = format!("{}\t", id.trim());
        if let Some(line) = list_str.lines().find(|l| l.starts_with(&prefix)) {
            if let Ok(mut c) = Command::new("cliphist").arg("delete").stdin(Stdio::piped()).spawn() {
                let _ = writeln!(c.stdin.take().unwrap(), "{}", line);
                let _ = c.wait();
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let cache_dir = dirs::cache_dir().unwrap().join("cliphist-tui");
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
        Some(Commands::Run) => run_tui(),
        Some(Commands::List) => {
            let mut stdout = std::io::stdout();
            stream_formatted_list(&mut stdout);
        }
        Some(Commands::Preview { id }) => run_preview(id, &cache_dir),
        Some(Commands::Copy { id }) => run_copy(id),
        Some(Commands::Open { id }) => run_open(id, &cache_dir),
        Some(Commands::Delete { id }) => delete_clip_by_id(id),
        Some(Commands::DeleteAll) => {
            let db_path = dirs::cache_dir().unwrap().join("cliphist/db");
            let _ = Command::new("gio").arg("trash").arg(&db_path).status();
            let _ = fs::remove_file(&db_path);
            if env::var("ENABLE_ICAT").unwrap_or_default() == "1" {
                if let Ok(mut tty) = fs::OpenOptions::new().write(true).open("/dev/tty") {
                    let _ = tty.write_all(b"\x1B_Ga=d,d=A\x1B\\");
                }
            }
        }
        None => run_tui(),
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

// 彻底改为管道流式处理，零等待直接投递，将最大内存消耗平摊掉
fn stream_formatted_list<W: Write>(mut writer: W) {
    if let Ok(mut child) = Command::new("cliphist").arg("list").stdout(Stdio::piped()).spawn() {
        if let Some(stdout) = child.stdout.take() {
            let reader = BufReader::new(stdout);
            let mut num = 1;
            
            for line in reader.lines().map_while(Result::ok) {
                if line.contains("<html") && line.contains("[表情]") { continue; }
                if let Some((id, content)) = line.split_once('\t') {
                    let formatted = format_content(content);
                    let _ = writeln!(writer, "{}\t\x1b[90m{:<2} \x1b[0m{}", id, num, formatted);
                    num += 1;
                }
            }
        }
        let _ = child.wait();
    }
}

fn format_content(c: &str) -> String {
    let t = "\x1b[1;35m"; let p = "\x1b[1;34m"; let cy = "\x1b[1;36m"; let r = "\x1b[0m";
    fn get_ext(s: &str) -> &str { s.rsplit('.').next().unwrap_or("") }
    let lower_c = c.to_lowercase(); // 提前小写避免重复转换

    if is_video_ext(c) {
        if c.starts_with("file://") { return format!("{t}[VIDEO]Url.{}{r}", get_ext(c)); } 
        else { return format!("{t}[VIDEO]File.{}{r}", get_ext(c)); }
    }
    if c.contains("QQRichEditFormat") && c.contains("EditElement type=\"7\"") { return format!("{t}[VIDEO_HTML]QQ{r}"); }
    if c.contains("src=\"file://") && (c.contains("qq") || c.contains("QQ")) { return format!("{p}[IMG_HTML]QQ{r}"); }
    if c.starts_with("file://") && c.contains("xwechat") && c.contains("temp") { return format!("{p}[IMG_URL]WeChat{r}"); }
    if c.starts_with("file://") && lower_c.ends_with(".gif") { return format!("{p}[IMG]Url.gif{r}"); }
    if c.starts_with("file://") && (lower_c.ends_with(".png") || lower_c.ends_with(".jpg") || lower_c.ends_with(".webp")) { return format!("{t}[IMG]Url.{}{r}", get_ext(c)); }
    if c.starts_with("file://") && c.contains(".config/QQ/") { return format!("{p}[IMG_URL]QQ{r}"); }
    if c.starts_with("file://") { return format!("{cy}[URL]File{r}"); }
    if c.starts_with('/') && lower_c.ends_with(".gif") { return format!("{p}[IMG]Path.gif{r}"); }
    if c.starts_with('/') && (lower_c.ends_with(".png") || lower_c.ends_with(".jpg") || lower_c.ends_with(".webp")) { return format!("{t}[IMG]Path.{}{r}", get_ext(c)); }
    
    // 移除了在循环体内会导致致命 CPU 开销的 Regex 编译，改用原生字符串高速检测
    if c.starts_with("[[ binary data ") && c.ends_with(" ]]") {
        for ext in ["png", "jpg", "jpeg", "gif", "webp"] {
            if lower_c.contains(ext) { return format!("{t}[IMG]Bin.{}{r}", ext); }
        }
    }
    if c.contains("[[ binary data") { return format!("{cy}[BINARY]{r}"); }
    c.to_string()
}

fn run_tui() {
    let mut wait_timeout = 50;
    while wait_timeout > 0 {
        if let Ok((cols, lines)) = size() { if cols >= 35 && lines >= 25 { break; } }
        thread::sleep(Duration::from_millis(50));
        wait_timeout -= 1;
    }

    let exe = env::current_exe().unwrap().to_string_lossy().to_string();
    let port = rand::random::<u16>() % 50000 + 10000;
    
    // 【解决闪烁的终极修复】：在启动 watcher 之前，初始化 LAST_ID 文件
    // 否则 FZF 刚启动渲染完第一项，watcher 就会由于文件不存在立即触发一次 Reload，导致图片刚绘制就立刻被清除重绘。
    let last_id_path = format!("/dev/shm/shorinclip_last_id_{}", port);
    if let Ok(output) = Command::new("sh").arg("-c").arg("cliphist list | head -n 1 | cut -f1").output() {
        let initial_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let _ = fs::write(&last_id_path, &initial_id);
    }

    let watcher_script = format!(
        "for i in {{1..10}}; do \
            CURRENT_ID=$(cliphist list | head -n 1 | cut -f1); \
            LAST_ID=$(cat /dev/shm/shorinclip_last_id_{port} 2>/dev/null); \
            if [ -n \"$CURRENT_ID\" ] && [ \"$CURRENT_ID\" != \"$LAST_ID\" ]; then \
                echo \"$CURRENT_ID\" > /dev/shm/shorinclip_last_id_{port}; \
                curl -s -X POST -d 'reload({exe} list)' http://localhost:{port} & \
                break; \
            fi; \
            sleep 0.2; \
        done"
    );

    let mut watcher = Command::new("wl-paste").arg("--watch").arg("bash").arg("-c").arg(&watcher_script).spawn().unwrap();

    let mut fzf = Command::new("fzf")
        // 限制 Go 调度器使用最多 2 个系统线程，极大削减 fzf 的内存预分配和线程暴增
        .env("GOMAXPROCS", "2")
        .arg("--ansi").arg("--listen").arg(port.to_string())
        .arg(format!("--bind=ctrl-r:reload({exe} list)"))
        .arg(format!("--bind=ctrl-x:execute-silent({exe} delete {{1}})+reload({exe} list)"))
        .arg(format!("--bind=alt-x:execute-silent({exe} delete-all)+reload({exe} list)"))
        .arg(format!("--bind=ctrl-o:execute-silent({exe} open {{1}})"))
        .arg(format!("--bind=ctrl-e:execute-silent({exe} open {{1}})"))
        .arg("--prompt=󰅍 > ")
        .arg("--header=C^-X: Delete | Alt+X: D-All | C^-R: Reload | C^-O/E: Open | Enter/C^-F: Paste")
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
        // 使用 64KB 大块缓冲，极大降低内核态与用户态的上下文切换开销
        let mut writer = BufWriter::with_capacity(64 * 1024, stdin);
        stream_formatted_list(&mut writer);
        let _ = writer.flush();
    }

    fzf.wait().unwrap();
    let _ = watcher.kill();
    let _ = fs::remove_file(&last_id_path); // 退出后清理残留缓存
}

fn run_preview(id: &str, cache_dir: &PathBuf) {
    if env::var("ENABLE_ICAT").unwrap_or_default() == "1" {
        if let Ok(mut tty) = fs::OpenOptions::new().write(true).open("/dev/tty") {
            let _ = tty.write_all(b"\x1B_Ga=d,d=A\x1B\\");
        }
    }

    let raw_bytes = get_decode(id);
    if raw_bytes.is_empty() { return; } 

    let mime = get_mime(&raw_bytes);
    let text_cow = String::from_utf8_lossy(&raw_bytes);
    let decoded_text = text_cow.trim_end_matches(|c| c == '\n' || c == '\r');

    if mime.starts_with("image/") {
        let ext = mime.split('/').last().unwrap_or("png");
        let hash = format!("{:x}", xxhash_rust::xxh3::xxh3_128(&raw_bytes));
        let cache_file = cache_dir.join(format!("{}.{}", hash, ext));
        if !cache_file.exists() { fs::write(&cache_file, &raw_bytes).unwrap(); }
        _preview(&cache_file);
    } else if mime == "text/html" && decoded_text.contains("QQ") {
        let re = Regex::new(r#"<img sr[c]="file://([^"]+)""#).unwrap();
        if let Some(caps) = re.captures(decoded_text) {
            if PathBuf::from(&caps[1]).exists() { _preview(Path::new(&caps[1])); return; }
        }
        println!("{}", decoded_text);
    } else if decoded_text.contains("QQRichEditFormat") && decoded_text.contains("EditElement type=\"7\"") {
        let re = Regex::new(r#"filepath="([^"]+)""#).unwrap();
        if let Some(caps) = re.captures(decoded_text) {
            if PathBuf::from(&caps[1]).exists() { _preview_video(&caps[1], cache_dir); return; }
        }
        println!("{}", decoded_text);
    } else if decoded_text.starts_with('/') && PathBuf::from(decoded_text).exists() {
        let p = PathBuf::from(decoded_text);
        if is_video_ext(decoded_text) {
            _preview_video(decoded_text, cache_dir);
        } else {
            let p_mime = get_mime_from_path(&p);
            if p_mime.starts_with("image/") { _preview(&p); } 
            else { println!("{}", decoded_text); }
        }
    } else if decoded_text.starts_with("file://") {
        let raw_path = urlencoding::decode(decoded_text.strip_prefix("file://").unwrap_or(decoded_text)).unwrap().into_owned();
        let p = PathBuf::from(&raw_path);
        if is_video_ext(&raw_path) {
            _preview_video(&raw_path, cache_dir);
        } else {
            let p_mime = get_mime_from_path(&p);
            if p_mime.starts_with("image/") { _preview(&p); } 
            else { println!("{}", decoded_text); }
        }
    } else {
        println!("{}", decoded_text);
    }
}

fn run_copy(id: &str) {
    let raw_bytes = get_decode(id);
    let mime = get_mime(&raw_bytes);
    let text_cow = String::from_utf8_lossy(&raw_bytes);
    let text = text_cow.trim_end_matches(|c| c == '\n' || c == '\r');

    if mime.starts_with("image/") {
        wl_copy(&raw_bytes, None);
        Command::new("notify-send").args(["Copied Image", id]).spawn().ok();
    } else if mime == "text/html" && text.contains("<img src=\"file://") {
        let re = Regex::new(r#"<img sr[c]="file://([^"]+)""#).unwrap();
        if let Some(caps) = re.captures(text) {
            let encoded = format!("file://{}", encode_path(&caps[1]));
            wl_copy(encoded.as_bytes(), Some("text/uri-list"));
            Command::new("notify-send").args(["Copied QQ Link", &caps[1]]).spawn().ok();
        }
    } else if text.contains("QQRichEditFormat") && text.contains("EditElement type=\"7\"") {
        let re = Regex::new(r#"filepath="([^"]+)""#).unwrap();
        if let Some(caps) = re.captures(text) {
            let encoded = format!("file://{}", encode_path(&caps[1]));
            wl_copy(encoded.as_bytes(), Some("text/uri-list"));
            Command::new("notify-send").args(["Copied QQ Video", &caps[1]]).spawn().ok();
        }
    } else if text.starts_with("file://") {
        wl_copy(text.as_bytes(), Some("text/uri-list"));
        Command::new("notify-send").args(["Copied File Link", text]).spawn().ok();
    } else if text.starts_with('/') && PathBuf::from(text).exists() {
        let encoded = format!("file://{}", encode_path(text));
        wl_copy(encoded.as_bytes(), Some("text/uri-list"));
        Command::new("notify-send").args(["Copied File Path", text]).spawn().ok();
    } else {
        wl_copy(&raw_bytes, None); 
    }
    
    delete_clip_by_id(id);
}

fn run_open(id: &str, cache_dir: &PathBuf) {
    let raw_bytes = get_decode(id);
    let mime = get_mime(&raw_bytes);
    let text_cow = String::from_utf8_lossy(&raw_bytes);
    let text = text_cow.trim_end_matches(|c| c == '\n' || c == '\r');

    if mime.starts_with("image/") {
        let hash = format!("{:x}", xxhash_rust::xxh3::xxh3_128(&raw_bytes));
        let ext = mime.split('/').last().unwrap_or("png");
        let file = cache_dir.join(format!("{}.{}", hash, ext));
        if !file.exists() { fs::write(&file, &raw_bytes).unwrap(); }
        Command::new("xdg-open").arg(&file).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok();
    } else if mime == "text/html" && text.contains("<img src=\"file://") {
        let re = Regex::new(r#"<img sr[c]="file://([^"]+)""#).unwrap();
        if let Some(caps) = re.captures(text) { Command::new("xdg-open").arg(&caps[1]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok(); }
    } else if text.contains("QQRichEditFormat") && text.contains("EditElement type=\"7\"") {
        let re = Regex::new(r#"filepath="([^"]+)""#).unwrap();
        if let Some(caps) = re.captures(text) { smart_open(&caps[1]); }
    } else if text.starts_with("file://") {
        let raw_path = urlencoding::decode(text.strip_prefix("file://").unwrap_or(text)).unwrap().into_owned();
        if PathBuf::from(&raw_path).exists() { smart_open(&raw_path); } 
        else { Command::new("notify-send").args(["Open Error", &format!("File missing: {}", raw_path)]).spawn().ok(); }
    } else if text.starts_with('/') && PathBuf::from(text).exists() {
        smart_open(text);
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

fn smart_open(path: &str) {
    let is_video = is_video_ext(path);
    let has_mpv = Command::new("which").arg("mpv").output().map(|o| o.status.success()).unwrap_or(false);
    
    if is_video && has_mpv {
        Command::new("mpv")
            .arg("--wayland-app-id=floating-mpv")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    } else {
        Command::new("xdg-open")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    }
}

fn _preview(path: &Path) {
    let is_kitty = env::var("ENABLE_ICAT").unwrap_or_default() == "1";
    let is_native = env::var("IS_NATIVE_KITTY").unwrap_or_default() == "1";
    let mime = get_mime_from_path(path);
    let cols = env::var("FZF_PREVIEW_COLUMNS").unwrap_or_else(|_| "80".to_string());
    let lines = env::var("FZF_PREVIEW_LINES").unwrap_or_else(|_| "24".to_string());

    if is_kitty && is_native && mime == "image/gif" {
        let mut cmd = Command::new("kitty");
        cmd.args(["icat", "--transfer-mode=file", "--image-id=10", &format!("--place={}x{}@0x0", cols, lines)]).arg(path);
        if let Ok(tty) = fs::OpenOptions::new().read(true).open("/dev/tty") { cmd.stdin(Stdio::from(tty)); }
        let _ = cmd.status();
    } else if is_kitty {
        let _ = Command::new("chafa").args(["-f", "kitty", "--animate=off", &format!("--size={}x{}", cols, lines)]).arg(path).status();
    } else {
        let _ = Command::new("chafa").args(["--animate=off", &format!("--size={}x{}", cols, lines)]).arg(path).status();
    }
}

fn _preview_video(vid: &str, cache_dir: &PathBuf) {
    let hash = format!("{:x}", xxhash_rust::xxh3::xxh3_128(vid.as_bytes()));
    let thumb = cache_dir.join(format!("{}.png", hash));
    
    if !thumb.exists() {
        let _ = Command::new("ffmpegthumbnailer")
            .args(["-i", vid, "-o", thumb.to_str().unwrap(), "-s", "0", "-t", "0"])
            .output();
    }
    
    if thumb.exists() && fs::metadata(&thumb).map(|m| m.len() > 0).unwrap_or(false) { 
        _preview(&thumb); 
    } else { 
        println!("Video: {} (No thumbnail)", vid); 
    }
}
