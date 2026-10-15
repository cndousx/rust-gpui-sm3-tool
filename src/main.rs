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
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(target_os = "windows")]
use std::cell::RefCell;
#[cfg(target_os = "windows")]
use windows::Win32::{
    Foundation::HWND,
    System::Com::{CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx},
    UI::Shell::{ITaskbarList3, TBPFLAG, TaskbarList},
};

/// 单位
///
/// - GB（Gigabyte）：按十进制，1 GB = 10³ MB = 10⁹ 字节 = 1,000,000,000 B
///
/// - GiB（Gibibyte）：按二进制，1 GiB = 2¹⁰ MiB = 2³⁰ 字节 = 1,073,741,824 B
const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];

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

    /// 获取进度信息
    ///
    /// string 文本 计算进度以及文件大小
    ///
    /// u32 进度百分比的整数部分
    fn get_progress_info(&self) -> (String, f32) {
        if let Some(hash) = &self.file_sm3_hash {
            (hash.clone(), 100.)
        } else if let (Some(progress), Some(total)) = (&self.progress, self.file_byte_len) {
            let done = progress.load(Ordering::Relaxed);
            let percent = if total == 0 {
                0
            } else {
                (done * 100 / total) as u32
            };
            (
                format!(
                    "计算中... {}% ({} / {})",
                    percent,
                    format_bytes(done as usize),
                    format_bytes(total as usize)
                ),
                percent as f32,
            )
        } else {
            ("尚未选择文件".to_string(), 0.)
        }
    }

    fn format_bytes_len(&self) -> String {
        format_bytes(self.file_byte_len.unwrap_or(0) as usize)
    }
}

fn format_bytes(size: usize) -> String {
    // 只要文件大小不超过 9 PB，用 f64是完全精确的
    let mut s = size as f64;

    let mut unit_idx = 0;
    while s >= 1024. && unit_idx < UNITS.len() - 1 {
        s /= 1024.;
        unit_idx += 1;
    }

    let precision = if s < 10. {
        2
    } else if s < 100. {
        1
    } else {
        0
    };
    format!("{:.prec$} {}", s, UNITS[unit_idx], prec = precision)
}

/// 进度条
///
/// [`progress`]取值是0到100
///
fn sm3_progress_bar(progress: f32, is_dark: bool) -> impl IntoElement {
    if progress < 0. || progress > 100. {
        panic!("param error")
    }
    let pct = (progress / 100.).clamp(0., 1.);
    div()
        .w_full()
        .h(px(6.))
        .rounded_full()
        .bg(if is_dark {
            rgb(0x374151)
        } else {
            rgb(0xa0acbf)
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
                .bg(if is_dark {
                    rgb(0xfbbf24)
                } else {
                    rgb(0xe68900)
                }), // 填充色黄
        )
}

impl Render for FilePickerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_calculating = self.is_calculating();
        let is_dark = match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => true,
            _ => false,
        };
        div()
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(if is_dark {
                rgb(0x1e1e1e) // 深色背景
            } else {
                rgb(0xf8f9fa) // 浅色背景
            })
            .text_color(if is_dark {
                rgb(0xffffff)
            } else {
                rgb(0xbfdbfe)
            })
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_8()
                    .child(
                        div()
                            .id("pick_file")
                            .px_8()
                            .py_1()
                            .rounded_lg()
                            .text_xl()
                            .cursor(if is_calculating {
                                CursorStyle::OperationNotAllowed // 🚫 禁用符号
                            } else {
                                CursorStyle::PointingHand // 👆 小手
                            })
                            .bg(if is_calculating {
                                rgb(0x64748b)
                            } else {
                                rgb(0x3b82f6)
                            })
                            .hover(|s| {
                                if !is_calculating {
                                    s.bg(rgb(0x2563eb))
                                } else {
                                    s
                                }
                            })
                            .active(|s| {
                                if !is_calculating {
                                    s.bg(rgb(0x1e40af))
                                } else {
                                    s
                                }
                            })
                            .child(if is_calculating {
                                "计算中... (请等待)"
                            } else {
                                "选择 / 拖入文件"
                            })
                            .when(!is_calculating, |this| {
                                this.on_click(cx.listener(move |this, _, _window, cx| {
                                    if !is_calculating {
                                        if let Some(path) =
                                            FileDialog::new().set_title("选择一个文件").pick_file()
                                        {
                                            this.calculate_sm3_hash(path, cx);
                                        }
                                    }
                                }))
                            }),
                    )
                    .child(if let Some(path) = &self.selected_file {
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_3()
                            .w_full()
                            .max_w(px(700.))
                            .min_w(px(700.))
                            .child(
                                div()
                                    .text_color(if is_dark {
                                        rgb(0x22c55e)
                                    } else {
                                        rgb(0x17843f)
                                    })
                                    .child(format!(
                                        "文件: {}",
                                        path.file_name().unwrap().to_str().unwrap()
                                    )),
                            )
                            .child(
                                div()
                                    .text_color(if is_dark {
                                        rgb(0x60a5fa)
                                    } else {
                                        rgb(0x167bf8)
                                    })
                                    .child(format!("大小: {}", self.format_bytes_len())),
                            )
                            .child(if let Some(hash) = &self.file_sm3_hash {
                                let sm3 = String::from(hash.as_str());
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .w_full()
                                    .child(
                                        div()
                                            .text_color(if is_dark {
                                                rgb(0xfbbf24)
                                            } else {
                                                rgb(0xe68900)
                                            })
                                            .child(format!("SM3: {}", &sm3)),
                                    )
                                    .child(
                                        div()
                                            .id("copy_btn")
                                            .px_2()
                                            .py_0()
                                            .rounded_lg()
                                            .bg(rgb(0x1e40af))
                                            .text_color(rgb(0xbfdbfe))
                                            .text_sm()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(0x2563eb)))
                                            .active(|s| s.bg(rgb(0x1e3a8a)))
                                            .child("复制")
                                            .on_click(cx.listener(move |this, _, _window, cx| {
                                                cx.write_to_clipboard(
                                                    gpui::ClipboardItem::new_string(sm3.clone()),
                                                );
                                                this.show_toast("SM3已复制到剪贴板", cx);
                                            })),
                                    )
                            } else {
                                let (text, percent) = self.get_progress_info();
                                // 纵向叠: 文字 + 进度条
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .w_full()
                                    .child(
                                        div()
                                            .text_color(if is_dark {
                                                rgb(0xfbbf24)
                                            } else {
                                                rgb(0xe68900)
                                            })
                                            .child(format!("SM3: {}", text)),
                                    )
                                    .child(sm3_progress_bar(percent, is_dark))
                            })
                    } else {
                        div()
                            .text_color(rgb(0x888888))
                            .text_lg()
                            .child("尚未选择文件")
                    }),
            )
            // --- Toast 浮动提示---
            .children(self.toast.as_ref().map(|(msg, _)| {
                div()
                    .absolute()
                    .top(px(16.))
                    .right(px(16.))
                    .px_4()
                    .py_2()
                    .rounded_lg()
                    .shadow_lg()
                    .bg(if is_dark {
                        rgb(0x2d3748)
                    } else {
                        rgb(0xffffff)
                    })
                    .border_1()
                    .border_color(if is_dark {
                        rgb(0x4a5568)
                    } else {
                        rgb(0xe2e8f0)
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                // 使用绿色勾号文本，避免 SVG 方法缺失
                                div()
                                    .size(px(20.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_color(rgb(0x48bb78))
                                            .text_lg()
                                            .font_weight(FontWeight::BOLD)
                                            .child("✓"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_color(if is_dark {
                                        rgb(0xedf2f7)
                                    } else {
                                        rgb(0x2d3748)
                                    })
                                    .text_sm()
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
                    title: Some("SM3摘要计算工具".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(750.), px(450.)),
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
