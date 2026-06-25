#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use gpui::*;
use rfd::FileDialog;
use serde::{Deserialize, Serialize};
use sm3::{Digest, Sm3};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{self, SyncSender};
use std::thread;
struct FilePickerApp {
    selected_file: Option<PathBuf>,
    file_sm3_hash: Option<String>,
    file_byte_len: Option<u64>,
    channel: Option<(SyncSender<Message>, Receiver<Message>)>,
}

impl FilePickerApp {
    fn new() -> Self {
        Self {
            selected_file: None,
            file_sm3_hash: None,
            file_byte_len: None,
            channel: Some(mpsc::sync_channel(1)), // 指定管道容量，如果是1，则计算sm3会很慢，指定太大，页面上又会看不到进度
        }
    }
    /// 计算文件sm3摘要
    fn calculate_sm3_hash(&mut self, path: &PathBuf) {
        let (tx, _rx) = self.channel.as_ref().unwrap();
        self.file_sm3_hash = None;
        match File::open(path) {
            Ok(mut file) => {
                let total = file.metadata().unwrap().len() as usize;
                let sender = tx.clone();
                thread::spawn(move || {
                    let mut hasher = Sm3::new();
                    let mut bf = [0u8; 64 * 1024];
                    let mut read_total = 0;
                    let mut pre = 0.0f64;
                    // 初始化
                    let _ = sender.send(Message::Progress(0, total));
                    loop {
                        match file.read(&mut bf) {
                            Ok(read) => {
                                if read == 0 {
                                    break;
                                }
                                hasher.update(&bf[..read]);
                                read_total += read;
                                let curr = read_total as f64 / total as f64;
                                if curr - pre >= 0.01 || read_total == total {
                                    // 分成100份发消息
                                    pre = curr;
                                    let _ = sender.send(Message::Progress(read_total, total));
                                }
                            }
                            Err(e) => {
                                // self.file_sm3_hash = None;
                                // self.file_size = None;
                                panic!("read err {}", e);
                            }
                        }
                    }
                    let result = hasher.finalize();
                    let sm3_hash = format!("{}", hex::encode(result));
                    let _ = sender.send(Message::SM3(sm3_hash));
                });
            }
            Err(e) => {
                self.file_sm3_hash = None;
                self.file_byte_len = None;
                eprintln!("open file err {}", e);
            }
        }
    }

    /// 将字节数转换为人类可读的字符串（自动选择合适的单位）
    fn format_bytes_len(&self) -> String {
        format_bytes(self.file_byte_len.map_or(0, |s| s) as usize)
    }
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
enum Message {
    Progress(usize, usize),
    SM3(String),
}
/// 将字节数转换为人类可读的字符串（自动选择合适的单位）
fn format_bytes(size: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    let mut unit_idx = 0;
    let mut size = size as f64;

    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }

    // 根据大小决定小数位数：小于10保留两位，否则保留一位或零位
    let precision = if size < 10.0 {
        2
    } else if size < 100.0 {
        1
    } else {
        0
    };
    format!("{:.prec$} {}", size, UNITS[unit_idx], prec = precision)
}
impl Render for FilePickerApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // if let Some((_, ref rx)) = self.channel {
        //     if let Ok(msg) = rx.try_recv() {
        //         match msg {
        //             Message::Progress(read_total, total) => {
        //                 self.file_byte_len = Some(total as u64);
        //                 self.file_sm3_hash = Some(format!(
        //                     "计算中... {} / {}",
        //                     format_bytes(read_total),
        //                     format_bytes(total)
        //                 ));
        //             }
        //             Message::SM3(hash) => {
        //                 self.file_sm3_hash = Some(hash);
        //             }
        //         }
        //         cx.notify(); // 触发下一次 render，继续消费下一条
        //     }
        // }

        if let Some((_, ref rx)) = self.channel {
            // while和if let 似乎没有区别
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Message::Progress(read_total, total) => {
                        self.file_byte_len = Some(total as u64);
                        self.file_sm3_hash = Some(format!(
                            "计算中... {} / {}",
                            format_bytes(read_total),
                            format_bytes(total)
                        ));
                    }
                    Message::SM3(hash) => {
                        self.file_sm3_hash = Some(hash);
                    }
                }
                cx.notify();
            }
        }

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_6()
                    .child(
                        div()
                            .id("pick_file")
                            .px_8()
                            .py_1()
                            .bg(rgb(0x3b82f6))
                            .rounded_md()
                            .hover(|s| s.bg(rgb(0x2563eb)))
                            .active(|s| s.bg(rgb(0x1e40af)))
                            .cursor_pointer()
                            .child("选择文件")
                            .on_click(cx.listener(|this, _, _window, _cx| {
                                let path = FileDialog::new().set_title("选择一个文件").pick_file();
                                if let Some(path) = path {
                                    this.selected_file = Some(path.clone());
                                    this.calculate_sm3_hash(&path);
                                }
                            })),
                    )
                    .child(if let Some(path) = &self.selected_file {
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(rgb(0x22c55e))
                                    .child(format!("文件: {}", path.display())),
                            )
                            .child(
                                div()
                                    .text_color(rgb(0x60a5fa))
                                    .child(format!("大小: {} ", self.format_bytes_len())),
                            )
                            .child(div().text_color(rgb(0xfbbf24)).child(format!(
                                "SM3: {}",
                                self.file_sm3_hash.as_deref().unwrap_or("计算中...")
                            )))
                    } else {
                        div().text_color(rgb(0x888888)).child("尚未选择文件")
                    }),
            )
    }
}
fn main() {
    gpui_platform::application().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("计算SM3摘要".into()),
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(800.), px(500.)),
                    cx,
                ))),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| FilePickerApp::new()),
        )
        .unwrap();
    });
}
