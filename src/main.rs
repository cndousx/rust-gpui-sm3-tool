#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::CursorStyle;
use gpui::prelude::FluentBuilder;
use gpui::*;
use rfd::FileDialog;
use sm3::Digest;
use sm3::Sm3;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

#[cfg(target_os = "windows")]
use std::cell::RefCell;
#[cfg(target_os = "windows")]
use windows::Win32::{
    Foundation::HWND,
    System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx},
    UI::Shell::{ITaskbarList3, TBPFLAG, TaskbarList},
};

/// 文件大小单位
///
/// 数值按 1024 进制换算，单位使用界面上常见的 KB / MB / GB 写法。
const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];

// 界面图标（内嵌 SVG，GPUI 会按 text_color 着色）
const ICON_FILE: &[u8] = include_bytes!("../assets/icons/file.svg");
const ICON_FOLDER: &[u8] = include_bytes!("../assets/icons/folder.svg");
const ICON_COPY: &[u8] = include_bytes!("../assets/icons/copy.svg");

/// 自绘标题栏高度
const TITLEBAR_HEIGHT: f32 = 40.;

/// 应用图标（内嵌 ICO，解码一次后由 GPUI 资源缓存复用）
fn app_icon() -> Arc<Image> {
    static ICON: OnceLock<Arc<Image>> = OnceLock::new();
    ICON.get_or_init(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Ico,
            include_bytes!("../assets/app.ico").to_vec(),
        ))
    })
    .clone()
}

/// 自绘标题栏上的一个窗口控制按钮
///
/// `WindowControlArea` 让 Windows 后端用非客户区命中测试来处理点击：
/// Min 最小化、Max 最大化/还原、Close 关闭，行为与系统标题栏一致
/// （包括最大化按钮的“还原”状态和关闭前的确认流程）。
fn window_control_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    is_close: bool,
    is_dark: bool,
    text_color: Hsla,
) -> impl IntoElement {
    let hover_bg: Hsla = if is_close {
        rgb(0xe81123).into()
    } else if is_dark {
        rgb(0x32353a).into()
    } else {
        rgb(0xe5e7eb).into()
    };
    let active_bg: Hsla = if is_close {
        rgb(0xf1707a).into()
    } else if is_dark {
        rgb(0x3f4248).into()
    } else {
        rgb(0xd1d5db).into()
    };
    let hover_fg: Hsla = if is_close {
        rgb(0xffffff).into()
    } else {
        text_color
    };

    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(46.))
        .h_full()
        // occlude 让按钮自身的命中盒优先于父级标题栏的 Drag 区域
        .occlude()
        .font_family("Segoe MDL2 Assets")
        .text_size(px(10.))
        .text_color(text_color)
        .hover(move |style| style.bg(hover_bg).text_color(hover_fg))
        .active(move |style| style.bg(active_bg).text_color(hover_fg))
        .window_control_area(area)
        .child(glyph)
}

/// 自绘标题栏：左侧应用名，右侧最小化 / 最大化（还原）/ 关闭
fn app_titlebar(
    is_dark: bool,
    is_maximized: bool,
    muted_color: Hsla,
    body_color: Hsla,
) -> impl IntoElement {
    div()
        .id("titlebar")
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .h(px(TITLEBAR_HEIGHT))
        .w_full()
        .pl(px(14.))
        // 整条标题栏作为拖动区域：拖动移动窗口，双击最大化/还原
        .window_control_area(WindowControlArea::Drag)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    img(app_icon())
                        .flex_none()
                        .w(px(18.))
                        .h(px(18.))
                        .object_fit(ObjectFit::Contain),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(muted_color)
                        .child("SM3摘要工具"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .h_full()
                .child(window_control_button(
                    "win_min",
                    "\u{e921}",
                    WindowControlArea::Min,
                    false,
                    is_dark,
                    body_color,
                ))
                .child(window_control_button(
                    if is_maximized {
                        "win_restore"
                    } else {
                        "win_max"
                    },
                    if is_maximized { "\u{e923}" } else { "\u{e922}" },
                    WindowControlArea::Max,
                    false,
                    is_dark,
                    body_color,
                ))
                .child(window_control_button(
                    "win_close",
                    "\u{e8bb}",
                    WindowControlArea::Close,
                    true,
                    is_dark,
                    body_color,
                )),
        )
}

// ---------------------------------------------------------------------------
// Windows 任务栏进度（ITaskbarList3）
// ---------------------------------------------------------------------------

/// 本窗口对应的任务栏进度控制器
///
/// 只保存 HWND 的整数值，因此是 Send + Sync，可以随意放进 Entity / Task。
/// 非 Windows 平台上所有方法都是空操作。
///
#[derive(Clone, Copy, Default)]
pub struct TaskbarProgress(Option<isize>);

impl TaskbarProgress {
    pub fn new(hwnd: Option<isize>) -> Self {
        Self(hwnd)
    }

    /// 确定进度：done / total 只取比例，传 (50,100) 和 (1,2) 等价
    pub fn set(&self, done: u64, total: u64) {
        #[cfg(target_os = "windows")]
        {
            if total == 0 {
                return;
            }
            self.call(|list, hwnd| unsafe {
                let _ = list.SetProgressValue(hwnd, done, total);
            });
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (done, total);
        }
    }

    /// 绿色正常进度（也是调用 set 时的隐含状态）
    pub fn normal(&self) {
        self.state(0x2) // TBPF_NORMAL
    }
    /// 走马灯，进度未知时使用
    pub fn indeterminate(&self) {
        self.state(0x1) // TBPF_INDETERMINATE
    }
    /// 黄色暂停
    pub fn paused(&self) {
        self.state(0x8) // TBPF_PAUSED
    }
    /// 红色错误
    pub fn error(&self) {
        self.state(0x4) // TBPF_ERROR
    }
    /// 收起进度条：任务完成 / 取消时必须调用，否则会一直卡在满格
    pub fn clear(&self) {
        self.state(0x0) // TBPF_NOPROGRESS
    }

    fn state(&self, flag: i32) {
        #[cfg(target_os = "windows")]
        {
            self.call(|list, hwnd| unsafe {
                let _ = list.SetProgressState(hwnd, TBPFLAG(flag));
            });
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = flag;
        }
    }

    #[cfg(target_os = "windows")]
    fn call(&self, f: impl FnOnce(&ITaskbarList3, HWND)) {
        let Some(raw) = self.0 else { return };
        let hwnd = HWND(raw as *mut std::ffi::c_void);
        if let Some(list) = taskbar_list() {
            f(&list, hwnd);
        }
    }
}

/// ITaskbarList3 是 STA COM 对象，用 thread_local 缓存在 UI 线程上，不要跨线程共享。
#[cfg(target_os = "windows")]
fn taskbar_list() -> Option<ITaskbarList3> {
    thread_local! {
        static TBL: RefCell<Option<ITaskbarList3>> = RefCell::new(None);
    }
    TBL.with(|cell| {
        if let Some(list) = cell.borrow().as_ref() {
            return Some(list.clone());
        }
        unsafe {
            // gpui 的 Windows 后端通常已经初始化过 COM，这里再调一次只是兜底，失败忽略
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let list: ITaskbarList3 = CoCreateInstance(&TaskbarList, None, CLSCTX_ALL).ok()?;
            let _ = list.HrInit();
            cell.borrow_mut().replace(list.clone());
            Some(list)
        }
    })
}

/// 从 gpui 的 Window 里取系统 HWND。
/// 注意：Window 既有同名的固有方法 window_handle()（返回 gpui 的 AnyWindowHandle），
/// 所以拿系统句柄必须全限定调用 trait 方法。
#[cfg(target_os = "windows")]
fn taskbar_of_window(window: &Window) -> TaskbarProgress {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let hwnd = match HasWindowHandle::window_handle(window) {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
            _ => None,
        },
        Err(_) => None,
    };
    TaskbarProgress::new(hwnd)
}

#[cfg(not(target_os = "windows"))]
fn taskbar_of_window(_window: &Window) -> TaskbarProgress {
    TaskbarProgress::new(None)
}

// ---------------------------------------------------------------------------

/// 半透明色：rgb + alpha（0.0~1.0）
fn rgba_u32(rgb_val: u32, alpha: f32) -> Hsla {
    let mut c = rgb(rgb_val);
    c.a = alpha.clamp(0.0, 1.0);
    c.into()
}

struct FilePickerApp {
    selected_file: Option<PathBuf>,
    file_byte_len: Option<usize>,
    file_sm3_hash: Option<String>,
    progress: Option<Arc<AtomicUsize>>,
    calculate_task: Option<Task<()>>,
    refresh_ui_task: Option<Task<()>>,
    toast: Option<(String, Task<()>)>,
    taskbar: TaskbarProgress,
    /// 文件正在拖到窗口上方（尚未松开）时为 true，用于显示遮罩文字
    is_dragging_over: bool,
    /// 本次计算开始的时间，用于界面上的计时显示
    started_at: Option<Instant>,
    /// 计算结束后的总耗时，保证完成后计时不再继续增长
    finished_elapsed: Option<Duration>,
}

impl FilePickerApp {
    fn new(taskbar: TaskbarProgress) -> Self {
        Self {
            selected_file: None,
            file_byte_len: None,
            file_sm3_hash: None,
            progress: None,
            calculate_task: None,
            refresh_ui_task: None,
            toast: None,
            taskbar,
            is_dragging_over: false,
            started_at: None,
            finished_elapsed: None,
        }
    }

    /// 处理外部文件放下：只取第一个文件（与“选择文件”行为一致）
    fn handle_file_drop(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        self.is_dragging_over = false;
        if self.is_calculating() {
            self.show_toast("正在计算中，请等待完成后再拖入文件", cx);
            return;
        }
        let path_list = paths.paths();
        if path_list.is_empty() {
            cx.notify();
            return;
        }
        // 多于 1 个文件时提示，仍只处理第一个
        if path_list.len() > 1 {
            self.show_toast("一次只能计算一个文件，已选取第一个", cx);
        }
        if let Some(path) = paths.paths().first() {
            // 只处理普通文件，忽略目录
            if path.is_file() {
                self.calculate_sm3_hash(path.clone(), cx);
            } else {
                self.show_toast("请拖入文件（不支持文件夹）", cx);
            }
        }
        cx.notify();
    }

    fn set_dragging_over(&mut self, over: bool, cx: &mut Context<Self>) {
        if self.is_dragging_over != over {
            self.is_dragging_over = over;
            cx.notify();
        }
    }

    fn is_calculating(&self) -> bool {
        self.progress.is_some()
    }

    /// 把当前进度同步到任务栏（由 100ms 的刷新任务调用）
    fn sync_taskbar(&self) {
        if let (Some(progress), Some(total)) = (&self.progress, self.file_byte_len) {
            if total > 0 {
                self.taskbar
                    .set(progress.load(Ordering::Relaxed) as u64, total as u64);
            }
        }
    }

    fn show_toast(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some((_, task)) = self.toast.take() {
            task.detach();
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            // 提示显示3秒，然后自动消失
            let _ = this.update(cx, |app, cx| {
                app.toast = None;
                cx.notify();
            });
        });
        self.toast = Some((message.to_string(), task));
        cx.notify();
    }

    fn calculate_sm3_hash(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.is_calculating() {
            return; // 防止重复点击
        }
        // 取消旧任务
        self.calculate_task = None;
        self.refresh_ui_task = None;
        self.file_sm3_hash = None;
        self.progress = None;
        self.is_dragging_over = false;

        let total_size = std::fs::metadata(&path)
            .map(|m| m.len() as usize)
            .unwrap_or(0usize);

        // 任务栏：先清掉上一次可能残留的 error/paused 状态，再从 0 开始
        self.taskbar.normal();
        if total_size > 0 {
            self.taskbar.set(0, total_size as u64);
        } else {
            self.taskbar.indeterminate();
        }

        let progress = Arc::new(AtomicUsize::new(0));
        self.progress = Some(Arc::clone(&progress));
        self.file_byte_len = Some(total_size);
        self.selected_file = Some(path.clone());
        let started_at = Instant::now();
        self.started_at = Some(started_at);
        self.finished_elapsed = None;

        self.calculate_task = Some(cx.spawn(async move |this, cx| {
            // 在后台线程执行计算
            let hash_result: Option<String> = cx
                .background_spawn(async move {
                    let mut file = match File::open(&path) {
                        Ok(f) => f,
                        Err(_) => return None,
                    };
                    let file_name = path.file_name().unwrap().to_str().unwrap();
                    let mut hasher = Sm3::new();
                    let mut buffer = vec![0u8; 512 * 1024]; // 512KB buffer
                    let mut read_total = 0usize;
                    let mut rate_pre = 0usize;
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => {
                                hasher.update(&buffer[..n]);
                                read_total += n;
                                // 进度百分比的整数部分
                                let rate = if total_size == 0 {
                                    0
                                } else {
                                    read_total * 100 / total_size
                                };
                                if rate - rate_pre >= 1 {
                                    // 这里每增加超过1%，应该让ui刷新一次。
                                    println!("[{file_name}]calculate progress: {rate}%");
                                    rate_pre = rate;
                                    progress.store(read_total, Ordering::Relaxed);
                                }
                            }
                            Err(_) => return None,
                        }
                    }

                    let result = hasher.finalize();
                    Some(hex::encode(result))
                })
                .await;

            // 更新 UI
            let _ = this.update(cx, |app, cx| {
                let ok = hash_result.is_some();
                app.finished_elapsed = Some(started_at.elapsed());
                app.file_sm3_hash = match hash_result {
                    Some(hash) => Some(hash),
                    None => Some("计算失败".to_string()),
                };
                app.progress = None;

                if ok {
                    // 补满 100%，让用户看到"完成"，1.5 秒后再收起
                    app.taskbar.set(total_size as u64, total_size as u64);
                    let tb = app.taskbar;
                    cx.spawn(async move |_this, cx| {
                        cx.background_executor()
                            .timer(Duration::from_millis(1500))
                            .await;
                        tb.clear();
                    })
                    .detach();
                } else {
                    // 失败：任务栏变红，保持到下一次操作
                    app.taskbar.error();
                }

                cx.notify();
            });
        }));
        // 说明：interval_ticker的间隔不易过短，设置过短会影响sm3计算速度。设置100毫秒(即1秒刷新10次UI)对计算影响不大
        let mut interval_ticker = tokio::time::interval(Duration::from_millis(100));
        self.refresh_ui_task = Some(cx.spawn(async move |this, cx| {
            loop {
                // 100毫秒刷新一次,需要tokio环境
                interval_ticker.tick().await;
                let is_done = this.update(cx, |app, cx| {
                    // 刷新ui + 刷新任务栏进度
                    app.sync_taskbar();
                    cx.notify();
                    !app.is_calculating()
                });
                if let Ok(true) = is_done {
                    println!("计算结束, refresh ui break[let ok]");
                    break;
                }
            }
        }));
    }

    /// 当前进度百分比（0~100 的整数部分）
    fn progress_percent(&self) -> u32 {
        if self.file_sm3_hash.is_some() {
            return 100;
        }
        if let (Some(progress), Some(total)) = (&self.progress, self.file_byte_len) {
            if total == 0 {
                0
            } else {
                (progress.load(Ordering::Relaxed) * 100 / total) as u32
            }
        } else {
            0
        }
    }

    /// 已处理字节数与文件总字节数
    fn progress_bytes(&self) -> (usize, usize) {
        let total = self.file_byte_len.unwrap_or(0);
        if self.file_sm3_hash.is_some() {
            (total, total)
        } else {
            (
                self.progress
                    .as_ref()
                    .map(|p| p.load(Ordering::Relaxed))
                    .unwrap_or(0),
                total,
            )
        }
    }

    /// 本次计算已用的时间，格式为 HH:MM:SS
    fn format_elapsed(&self) -> String {
        let secs = self
            .finished_elapsed
            .or_else(|| self.started_at.map(|start| start.elapsed()))
            .unwrap_or_default()
            .as_secs();
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }

    /// 文件大小详情，例如 "185.23 MB (194,225,123 字节)"
    fn format_bytes_detail(&self) -> String {
        let len = self.file_byte_len.unwrap_or(0);
        format!("{} ({} 字节)", format_bytes(len), format_thousands(len))
    }
}

/// 给整数加上千位分隔符
fn format_thousands(value: usize) -> String {
    let digits = value.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*byte as char);
    }
    out
}

fn format_bytes(size: usize) -> String {
    // 只要文件大小不超过 9 PB，用 f64是完全精确的
    let mut s = size as f64;

    let mut unit_idx = 0;
    while s >= 1024. && unit_idx < UNITS.len() - 1 {
        s /= 1024.;
        unit_idx += 1;
    }

    let precision = if unit_idx == 0 { 0 } else { 2 };
    format!("{:.prec$} {}", s, UNITS[unit_idx], prec = precision)
}

/// 进度条
///
/// [`progress`] 取值是 0 到 100
///
fn sm3_progress_bar(progress: f32, is_dark: bool, is_error: bool) -> impl IntoElement {
    if !(0. ..=100.).contains(&progress) {
        panic!("param error")
    }
    let pct = (progress / 100.).clamp(0., 1.);
    let fill = if is_error {
        if is_dark {
            rgb(0xf87171)
        } else {
            rgb(0xdc2626)
        }
    } else {
        rgb(0x3b82f6)
    };
    div()
        .w_full()
        .h(px(10.))
        .rounded_full()
        .bg(if is_dark {
            rgb(0x3a3d42)
        } else {
            rgb(0xe2e8f0)
        }) // 轨道色
        .relative()
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(relative(pct))
                .rounded_full()
                .bg(fill), // 填充色
        )
}

fn icon(data: &'static [u8], size: f32, color: Hsla) -> impl IntoElement {
    svg().data(data).w(px(size)).h(px(size)).text_color(color)
}

fn info_row(
    icon_data: &'static [u8],
    label: &'static str,
    value: String,
    icon_color: Hsla,
    text_color: Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .child(icon(icon_data, 19., icon_color))
        .child(
            div()
                .flex()
                .items_center()
                .min_w_0()
                .text_size(px(16.))
                .child(
                    div()
                        .flex_none()
                        .text_color(text_color)
                        .child(format!("{}：", label)),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(text_color)
                        .child(value),
                ),
        )
}

impl Render for FilePickerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_calculating = self.is_calculating();
        let is_dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );

        // 深色主题尽量贴近最终效果图；浅色主题使用对应的中性色。
        let (
            bg_color,
            title_color,
            subtitle_color,
            body_color,
            muted_color,
            card_bg,
            card_border,
            divider_color,
            subtle_bg,
            footer_color,
        ): (Hsla, Hsla, Hsla, Hsla, Hsla, Hsla, Hsla, Hsla, Hsla, Hsla) = if is_dark {
            (
                rgb(0x1b1c1f).into(),
                rgb(0xf4f4f5).into(),
                rgb(0x9a9da3).into(),
                rgb(0xe7e7ea).into(),
                rgb(0x8d9198).into(),
                rgb(0x212226).into(),
                rgb(0x2e3034).into(),
                rgb(0x2b2d31).into(),
                rgb(0x292b2f).into(),
                rgb(0x7f838a).into(),
            )
        } else {
            (
                rgb(0xf5f6f8).into(),
                rgb(0x111827).into(),
                rgb(0x6b7280).into(),
                rgb(0x1f2937).into(),
                rgb(0x6b7280).into(),
                rgb(0xffffff).into(),
                rgb(0xe5e7eb).into(),
                rgb(0xe5e7eb).into(),
                rgb(0xffffff).into(),
                rgb(0x9ca3af).into(),
            )
        };

        let success_color: Hsla = if is_dark {
            rgb(0x34d399).into()
        } else {
            rgb(0x059669).into()
        };
        let danger_color: Hsla = if is_dark {
            rgb(0xf87171).into()
        } else {
            rgb(0xdc2626).into()
        };
        let primary_color: Hsla = rgb(0x3b82f6).into();

        let has_file = self.selected_file.is_some();
        let hash = self.file_sm3_hash.clone();
        let is_failed = hash.as_deref() == Some("计算失败");
        let is_complete = hash.as_deref().is_some_and(|h| h != "计算失败");
        let percent = self.progress_percent();
        let (done_bytes, total_bytes) = self.progress_bytes();
        let percent_text = format!("{}%", percent);
        let progress_size_text = format!(
            "({} / {})",
            format_bytes(done_bytes),
            format_bytes(total_bytes)
        );
        let timer_text = self.format_elapsed();
        let file_name = self
            .selected_file
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("未知文件")
            .to_string();
        let file_detail = self.format_bytes_detail();

        let (status_label, status_color, hint_text, hint_color) = if is_failed {
            (
                "计算失败".to_string(),
                danger_color,
                "计算失败，请重新选择文件后重试",
                danger_color,
            )
        } else if is_complete {
            (
                "计算完成".to_string(),
                success_color,
                "计算完成，SM3摘要已生成",
                success_color,
            )
        } else {
            (
                "计算中...".to_string(),
                body_color,
                "正在计算SM3摘要，请稍候...",
                muted_color,
            )
        };

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(bg_color)
            .text_color(body_color)
            // 接收外部文件放下
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.handle_file_drop(paths, cx);
            }))
            // 拖拽在窗口内移动时：打开遮罩（ExternalPaths = 系统文件拖入）
            .on_drag_move(cx.listener(
                |this, _event: &DragMoveEvent<ExternalPaths>, _window, cx| {
                    this.set_dragging_over(true, cx);
                },
            ))
            // 拖出窗口（未放下）时关闭遮罩
            .on_file_drop_exit(cx.listener(|this, _event, _window, cx| {
                this.set_dragging_over(false, cx);
            }))
            // 悬停时改根节点样式（拖出窗口后 GPUI 会自动取消此样式）
            .drag_over::<ExternalPaths>(move |style, _paths, _window, _cx| {
                style.bg(if is_dark {
                    rgba_u32(0x1e3a8a, 0.55)
                } else {
                    rgba_u32(0x3b82f6, 0.45)
                })
            })
            // 自绘标题栏（替代 Windows 原生标题栏）
            .child(app_titlebar(
                is_dark,
                window.is_maximized(),
                muted_color,
                body_color,
            ))
            .child(
                div()
                    .id("main_scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .px(px(48.))
                    .pt(px(26.))
                    .pb(px(8.))
                    .items_center()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(800.))
                            .flex()
                            .flex_col()
                            // 标题
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(34.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(title_color)
                                            .child("SM3摘要计算工具"),
                                    )
                                    .child(
                                        div()
                                            .mt(px(8.))
                                            .text_size(px(18.))
                                            .text_color(subtitle_color)
                                            .child("选择文件并计算 SM3 摘要"),
                                    ),
                            )
                            // 选择文件按钮
                            .child(
                                div().mt(px(34.)).flex().justify_center().child(
                                    div()
                                        .id("pick_file")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .gap(px(9.))
                                        .w(px(200.))
                                        .h(px(52.))
                                        .rounded_lg()
                                        .text_size(px(17.))
                                        .text_color(rgb(0xffffff))
                                        .cursor(if is_calculating {
                                            CursorStyle::OperationNotAllowed
                                        } else {
                                            CursorStyle::PointingHand
                                        })
                                        .bg(if is_calculating {
                                            rgb(0x4b5563)
                                        } else {
                                            rgb(0x3b82f6)
                                        })
                                        .hover(move |style| {
                                            if is_calculating {
                                                style
                                            } else {
                                                style.bg(rgb(0x2f74e8))
                                            }
                                        })
                                        .active(move |style| {
                                            if is_calculating {
                                                style
                                            } else {
                                                style.bg(rgb(0x2563eb))
                                            }
                                        })
                                        .child(icon(ICON_FOLDER, 20., rgb(0xffffff).into()))
                                        .child(if is_calculating {
                                            "计算中..."
                                        } else {
                                            "选择文件"
                                        })
                                        .when(!is_calculating, |this| {
                                            this.on_click(cx.listener(
                                                move |this, _, _window, cx| {
                                                    if let Some(path) = FileDialog::new()
                                                        .set_title("选择一个文件")
                                                        .pick_file()
                                                    {
                                                        this.calculate_sm3_hash(path, cx);
                                                    }
                                                },
                                            ))
                                        }),
                                ),
                            )
                            // 文件信息 + 计算进度卡片
                            .children(has_file.then(|| {
                                div()
                                    .mt(px(34.))
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .rounded_xl()
                                    .border_1()
                                    .border_color(card_border)
                                    .bg(card_bg)
                                    .px(px(26.))
                                    .py(px(24.))
                                    .child(info_row(
                                        ICON_FILE,
                                        "文件名",
                                        file_name.clone(),
                                        muted_color,
                                        body_color,
                                    ))
                                    .child(div().mt(px(18.)).child(info_row(
                                        ICON_FILE,
                                        "文件大小",
                                        file_detail.clone(),
                                        muted_color,
                                        body_color,
                                    )))
                                    .child(div().my(px(22.)).h(px(1.)).w_full().bg(divider_color))
                                    .child(
                                        div()
                                            .text_size(px(16.))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(body_color)
                                            .child("SM3计算进度"),
                                    )
                                    .child(
                                        div()
                                            .mt(px(14.))
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap(px(16.))
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .text_size(px(16.))
                                                    .child(
                                                        div()
                                                            .text_color(status_color)
                                                            .child(status_label.clone()),
                                                    )
                                                    .child(
                                                        div()
                                                            .ml(px(6.))
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(primary_color)
                                                            .child(percent_text.clone()),
                                                    )
                                                    .child(
                                                        div().text_color(body_color).child(
                                                            format!(" {}", progress_size_text),
                                                        ),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_size(px(14.))
                                                    .text_color(muted_color)
                                                    .child(timer_text.clone()),
                                            ),
                                    )
                                    .child(div().mt(px(14.)).child(sm3_progress_bar(
                                        percent as f32,
                                        is_dark,
                                        is_failed,
                                    )))
                                    .child(
                                        div()
                                            .mt(px(14.))
                                            .text_size(px(14.))
                                            .text_color(hint_color)
                                            .child(hint_text),
                                    )
                            }))
                            // SM3 摘要结果
                            .children(hash.as_ref().map(|hash| {
                                let ok = hash != "计算失败";
                                let hash_for_copy = hash.clone();
                                div()
                                    .mt(px(42.))
                                    .w_full()
                                    .flex()
                                    .flex_col()
                                    .gap(px(14.))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(10.))
                                            .child(
                                                div()
                                                    .text_size(px(22.))
                                                    .text_color(muted_color)
                                                    .child("#"),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(18.))
                                                    .text_color(title_color)
                                                    .child("SM3摘要结果"),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(14.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .h(px(56.))
                                                    .px(px(20.))
                                                    .flex()
                                                    .items_center()
                                                    .rounded_lg()
                                                    .border_1()
                                                    .border_color(card_border)
                                                    .bg(subtle_bg)
                                                    .child(
                                                        div()
                                                            .text_size(px(14.))
                                                            .font_family("Consolas")
                                                            .truncate()
                                                            .text_color(if ok {
                                                                body_color
                                                            } else {
                                                                danger_color
                                                            })
                                                            .child(hash.clone()),
                                                    ),
                                            )
                                            .when(ok, |row| {
                                                row.child(
                                                    div()
                                                        .id("copy_btn")
                                                        .flex_none()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .gap(px(8.))
                                                        .w(px(116.))
                                                        .h(px(56.))
                                                        .rounded_lg()
                                                        .border_1()
                                                        .border_color(card_border)
                                                        .bg(subtle_bg)
                                                        .text_size(px(16.))
                                                        .text_color(body_color)
                                                        .cursor_pointer()
                                                        .hover(|style| {
                                                            style.bg(if is_dark {
                                                                rgb(0x33363b)
                                                            } else {
                                                                rgb(0xf1f5f9)
                                                            })
                                                        })
                                                        .active(|style| {
                                                            style.bg(if is_dark {
                                                                rgb(0x3b3e44)
                                                            } else {
                                                                rgb(0xe2e8f0)
                                                            })
                                                        })
                                                        .child(icon(ICON_COPY, 17., body_color))
                                                        .child("复制")
                                                        .on_click(cx.listener(
                                                            move |this, _, _window, cx| {
                                                                cx.write_to_clipboard(
                                                                    ClipboardItem::new_string(
                                                                        hash_for_copy.clone(),
                                                                    ),
                                                                );
                                                                this.show_toast(
                                                                    "SM3已复制到剪贴板",
                                                                    cx,
                                                                );
                                                            },
                                                        )),
                                                )
                                            }),
                                    )
                            })),
                    ),
            )
            // 底部说明
            .child(
                div()
                    .flex()
                    .justify_center()
                    .py(px(16.))
                    .text_size(px(13.))
                    .text_color(footer_color)
                    .child("Copyright (c) 2026 dousx"),
            )
            // --- Toast 浮动提示 ---
            .children(self.toast.as_ref().map(|(msg, _)| {
                div()
                    .absolute()
                    .top(px(TITLEBAR_HEIGHT + 12.))
                    .right(px(16.))
                    .px(px(16.))
                    .py(px(10.))
                    .rounded_lg()
                    .shadow_lg()
                    .bg(card_bg)
                    .border_1()
                    .border_color(card_border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .size(px(18.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_color(success_color)
                                            .text_size(px(16.))
                                            .font_weight(FontWeight::BOLD)
                                            .child("✓"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(body_color)
                                    .text_size(px(13.))
                                    .child(msg.clone()),
                            ),
                    )
            }))
    }
}

fn main() {
    // 创建一个 Tokio runtime，但不阻塞主线程
    let tokio_runtime = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
    // Runtime::new()返回的 Runtime对象必须保持存活（不能被 drop），否则 handle 会失效
    // 不能用链式调用调用enter(), 用链式调用tokio_runtime会被提前drop
    let _guard = tokio_runtime.enter(); // 只设置 handle，不阻塞
    gpui_platform::application().run(|cx| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("SM3摘要工具".into()),
                    // 隐藏系统标题栏，由应用自己绘制（Windows / macOS）
                    appears_transparent: true,
                    ..Default::default()
                }),
                // 窗口拖动由自绘标题栏负责（主要影响 macOS，Windows 由平台处理）
                app_owns_titlebar_drag: true,
                window_min_size: Some(size(px(640.), px(520.))),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(920.), px(800.)),
                    cx,
                ))),
                ..Default::default()
            },
            |window, cx| {
                // 窗口已创建，此时可以取到 HWND
                let taskbar = taskbar_of_window(window);
                let view = cx.new(|_| FilePickerApp::new(taskbar));
                // 监听系统主题变化
                window
                    .observe_window_appearance(|window, _cx| {
                        // 使用 window.refresh() 触发重绘
                        window.refresh();
                    })
                    .detach();
                view
            },
        )
        .unwrap();
    });
}
