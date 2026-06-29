#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gpui::prelude::FluentBuilder;
use gpui::*;
use rfd::FileDialog;
use rfd::{MessageDialog, MessageLevel};
use sm3::Digest;
use sm3::Sm3;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
struct FilePickerApp {
    selected_file: Option<PathBuf>,
    file_byte_len: Option<u64>,
    file_sm3_hash: Option<String>,
    progress: Option<Arc<AtomicU64>>,
    current_task: Option<Task<()>>,
}

impl FilePickerApp {
    fn new() -> Self {
        Self {
            selected_file: None,
            file_byte_len: None,
            file_sm3_hash: None,
            progress: None,
            current_task: None,
        }
    }

    fn is_calculating(&self) -> bool {
        self.progress.is_some()
    }

    fn calculate_sm3_hash(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.is_calculating() {
            return; // 防止重复点击
        }
        // 取消旧任务
        self.current_task = None;
        self.file_sm3_hash = None;
        self.progress = None;

        let total_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let progress = Arc::new(AtomicU64::new(0));
        self.progress = Some(Arc::clone(&progress));
        self.file_byte_len = Some(total_size);
        self.selected_file = Some(path.clone());

        let task = cx.spawn(async move |this, cx| {
            // 在后台线程执行计算
            let hash_result: Option<String> = cx
                .background_spawn(async move {
                    let mut file = match File::open(&path) {
                        Ok(f) => f,
                        Err(_) => return None,
                    };

                    let mut hasher = Sm3::new();
                    let mut buffer = vec![0u8; 512 * 1024]; // 512KB buffer
                    let mut read_total = 0usize;

                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => {
                                hasher.update(&buffer[..n]);
                                read_total += n;
                                progress.store(read_total as u64, Ordering::Relaxed);
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
                app.file_sm3_hash = match hash_result {
                    Some(hash) => Some(hash),
                    None => Some("计算失败".to_string()),
                };
                app.progress = None;
                cx.notify();
            });
        });

        self.current_task = Some(task);
    }

    fn get_progress_text(&self) -> String {
        if let Some(hash) = &self.file_sm3_hash {
            hash.clone()
        } else if let (Some(progress), Some(total)) = (&self.progress, self.file_byte_len) {
            let done = progress.load(Ordering::Relaxed);
            let percent = if total == 0 {
                0
            } else {
                ((done as f64 / total as f64) * 100.0) as u32
            };
            format!(
                "计算中... {}% ({} / {})",
                percent,
                format_bytes(done as usize),
                format_bytes(total as usize)
            )
        } else {
            "尚未选择文件".to_string()
        }
    }

    fn format_bytes_len(&self) -> String {
        format_bytes(self.file_byte_len.unwrap_or(0) as usize)
    }
}

fn format_bytes(size: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut s = size as f64;
    let mut unit_idx = 0;

    while s >= 1024.0 && unit_idx < UNITS.len() - 1 {
        s /= 1024.0;
        unit_idx += 1;
    }

    let precision = if s < 10.0 {
        2
    } else if s < 100.0 {
        1
    } else {
        0
    };
    format!("{:.prec$} {}", s, UNITS[unit_idx], prec = precision)
}

impl Render for FilePickerApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_calculating = self.is_calculating();
        let is_dark = match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => true,
            _ => false,
        };
        let bg_color = if is_dark {
            rgb(0x1e1e1e) // 深色背景
        } else {
            rgb(0xf8f9fa) // 浅色背景
        };

        let text_color = if is_dark {
            rgb(0xffffff)
        } else {
            rgb(0x111111)
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(bg_color)
            .text_color(text_color)
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
                            .cursor_pointer()
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
                                "选择文件"
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
                            .child(
                                div()
                                    .text_color(rgb(0x22c55e))
                                    .child(format!("文件: {}", path.display())),
                            )
                            .child(
                                div()
                                    .text_color(rgb(0x60a5fa))
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
                                            .text_color(rgb(0xfbbf24))
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
                                            .on_click(cx.listener(move |_, _, _window, cx| {
                                                cx.write_to_clipboard(
                                                    gpui::ClipboardItem::new_string(sm3.clone()),
                                                );

                                                MessageDialog::new()
                                                    .set_level(MessageLevel::Info)
                                                    .set_title("提示")
                                                    .set_description("SM3已复制到剪贴板")
                                                    .show();
                                            })),
                                    )
                            } else {
                                div()
                                    .text_color(rgb(0xfbbf24))
                                    .child(format!("SM3: {}", self.get_progress_text()))
                            })
                    } else {
                        div()
                            .text_color(rgb(0x888888))
                            .text_lg()
                            .child("尚未选择文件")
                    }),
            )
    }
}

fn main() {
    gpui_platform::application().run(|cx| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("SM3摘要计算工具".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(860.), px(520.)),
                    cx,
                ))),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(|_| FilePickerApp::new());
                // 监听系统主题变化
                window
                    .observe_window_appearance(|_window, _cx| {
                        // 使用 window.refresh() 触发重绘
                        _window.refresh();
                    })
                    .detach();

                view
            },
        )
        .unwrap();
    });
}
