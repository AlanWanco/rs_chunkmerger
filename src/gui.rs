use crate::gui_backend::{self, GuiPlan, GuiRequest};
use gpui_kit::base::{Disableable, Selectable};
use gpui_kit::component::{
    Root, Theme, ThemeMode,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use gpui_kit::{prelude::*, *};
use std::path::PathBuf;

fn parse_paths(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().trim_matches('"');
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            if line.starts_with("file://") {
                url::Url::parse(line).ok()?.to_file_path().ok()
            } else {
                Some(PathBuf::from(line))
            }
        })
        .collect()
}

fn clipboard_paths(item: &ClipboardItem) -> Vec<PathBuf> {
    let paths: Vec<_> = item
        .entries()
        .iter()
        .filter_map(|e| match e {
            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths().to_vec()),
            _ => None,
        })
        .flatten()
        .collect();
    if !paths.is_empty() {
        return paths;
    }
    parse_paths(&item.text().unwrap_or_default())
        .into_iter()
        .filter(|p| p.exists())
        .collect()
}

struct Merger {
    path: Entity<InputState>,
    start: Entity<InputState>,
    end: Entity<InputState>,
    imported: Vec<PathBuf>,
    ts: bool,
    recursive: bool,
    hidden: bool,
    split: bool,
    plan: Option<GuiPlan>,
    busy: bool,
    message: String,
    error: bool,
    clipboard_count: usize,
    subscriptions: Vec<Subscription>,
}
impl Merger {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = cx.new(|cx| {
            InputState::new(window, cx).placeholder("输入路径，或将文件 / 文件夹拖到这里")
        });
        let start = cx.new(|cx| InputState::new(window, cx).placeholder("首片 / 编号（可选）"));
        let end = cx.new(|cx| InputState::new(window, cx).placeholder("末片 / 编号（可选）"));
        let mut subscriptions = vec![];
        subscriptions.push(cx.subscribe(&path, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.imported.clear();
                this.invalidate(cx);
            }
        }));
        for input in [&start, &end] {
            subscriptions.push(cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.invalidate(cx);
                }
            }));
        }
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                if view
                    .update(cx, |this, cx| {
                        let count = cx
                            .read_from_clipboard()
                            .map(|item| clipboard_paths(&item).len())
                            .unwrap_or(0);
                        if count != this.clipboard_count {
                            this.clipboard_count = count;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            path,
            start,
            end,
            imported: vec![],
            ts: false,
            recursive: false,
            hidden: false,
            split: true,
            plan: None,
            busy: false,
            message: "导入分片后，先检测并核对合并范围。".into(),
            error: false,
            clipboard_count: 0,
            subscriptions,
        }
    }
    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.plan = None;
        self.error = false;
        self.message = "输入已更新，请重新检测。".into();
        cx.notify();
    }
    fn import(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || paths.is_empty() {
            return;
        }
        self.path.update(cx, |input, cx| {
            input.set_value(
                if paths.len() == 1 {
                    paths[0].display().to_string()
                } else {
                    format!("已选择 {} 个文件 / 目录", paths.len())
                },
                window,
                cx,
            )
        });
        self.imported = paths;
        self.invalidate(cx);
    }
    fn pick(&mut self, folder: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        cx.spawn(async move |view, cx| {
            let paths = cx
                .background_executor()
                .spawn(async move {
                    if folder {
                        rfd::FileDialog::new().pick_folder().into_iter().collect()
                    } else {
                        rfd::FileDialog::new()
                            .add_filter("媒体分片", &["ts", "decrypt", "mp4", "m4s"])
                            .pick_files()
                            .unwrap_or_default()
                    }
                })
                .await;
            let _ = view.update_in(cx, |this, window, cx| this.import(paths, window, cx));
        })
        .detach();
    }
    fn detect(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let paths = if self.imported.is_empty() {
            parse_paths(self.path.read(cx).value().as_ref())
        } else {
            self.imported.clone()
        };
        let optional = |input: &Entity<InputState>| {
            let value = input.read(cx).value().trim().to_string();
            if value.is_empty() { None } else { Some(value) }
        };
        let request = GuiRequest {
            paths,
            start: optional(&self.start),
            end: optional(&self.end),
            ts: self.ts,
            recursive: self.recursive,
            include_hidden: self.hidden,
            split_gaps: self.split,
        };
        self.busy = true;
        self.plan = None;
        self.error = false;
        self.message = "正在检测文件、编号与音视频轨…".into();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { gui_backend::prepare(request) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(plan) => {
                        this.message = format!(
                            "检测完成 · {} 个文件，{} 个输出。核对列表后确认合并。",
                            plan.files.len(),
                            plan.outputs.len()
                        );
                        this.plan = Some(plan);
                    }
                    Err(e) => {
                        this.error = true;
                        this.message = e.to_string();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn merge(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(plan) = self.plan.take() else {
            return;
        };
        self.busy = true;
        self.message = "正在合并，请等待写入与校验完成…".into();
        cx.notify();
        cx.spawn(async move |view, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { gui_backend::execute(plan) })
                .await;
            let _ = view.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(paths) => {
                        this.message = format!(
                            "合并完成\n{}",
                            paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    }
                    Err(e) => {
                        this.error = true;
                        this.message = format!("合并失败：{e}");
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
impl Render for Merger {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let paste = move |item: &ClipboardItem, window: &mut Window, cx: &mut App| {
            let paths = clipboard_paths(item);
            if paths.is_empty() {
                return false;
            }
            let _ = weak.update(cx, |this, cx| this.import(paths, window, cx));
            true
        };
        let mut preview = div().flex().flex_col().gap_2();
        if let Some(plan) = &self.plan {
            preview = preview
                .children(
                    plan.summary
                        .iter()
                        .map(|line| div().text_color(rgb(0xb6bfce)).child(line.clone())),
                )
                .child(
                    div()
                        .mt_3()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("输出文件"),
                )
                .children(plan.outputs.iter().map(|p| {
                    div()
                        .text_color(rgb(0x91baff))
                        .child(p.display().to_string())
                }))
                .child(
                    div()
                        .mt_3()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("合并顺序"),
                )
                .children(plan.files.iter().enumerate().map(|(i, p)| {
                    div()
                        .text_color(rgb(0xb6bfce))
                        .child(format!("{}   {}", i + 1, p))
                }));
        } else if !self.imported.is_empty() {
            preview = preview
                .child(div().font_weight(FontWeight::SEMIBOLD).child("已导入"))
                .children(self.imported.iter().map(|p| {
                    div()
                        .text_color(rgb(0xb6bfce))
                        .child(p.display().to_string())
                }));
        } else {
            preview = preview.child(
                div()
                    .text_color(rgb(0xb6bfce))
                    .child("拖入文件仅合并选中项；导入文件夹则扫描目录。"),
            );
        }
        let _ = &self.subscriptions;
        div()
            .size_full()
            .bg(rgb(0x15181e))
            .text_color(rgb(0xe8edf5))
            .text_sm()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_6()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(0x303641))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("分片合并"),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_color(rgb(0xb6bfce))
                            .child("将下载分片整理为完整视频"),
                    ),
            )
            .child(
                div()
                    .id("body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .p_4()
                            .bg(rgb(0x1c222c))
                            .border_1()
                            .border_color(rgb(0x3d526d))
                            .rounded_2xl()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        gpui_kit::component::Icon::new(
                                            gpui_kit::assets::IconName::FolderOpen,
                                        )
                                        .text_color(rgb(0x91baff)),
                                    )
                                    .child(
                                        div().font_weight(FontWeight::SEMIBOLD).child("导入分片"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0xb6bfce))
                                    .child("拖入文件或文件夹，也可以粘贴复制的文件"),
                            )
                            .child(
                                div()
                                    .id("drop-path")
                                    .on_drop(cx.listener(
                                        |this, paths: &ExternalPaths, window, cx| {
                                            this.import(paths.paths().to_vec(), window, cx)
                                        },
                                    ))
                                    .child(
                                        Input::new(&self.path)
                                            .rounded_xl()
                                            .disabled(self.busy)
                                            .on_paste(paste),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        Button::new("files")
                                            .rounded_full()
                                            .icon(gpui_kit::assets::IconName::File)
                                            .label("选择文件")
                                            .disabled(self.busy)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.pick(false, cx)),
                                            ),
                                    )
                                    .child(
                                        Button::new("folder")
                                            .rounded_full()
                                            .icon(gpui_kit::assets::IconName::FolderOpen)
                                            .label("选择目录")
                                            .disabled(self.busy)
                                            .on_click(
                                                cx.listener(|this, _, _, cx| this.pick(true, cx)),
                                            ),
                                    )
                                    .child(
                                        Button::new("paste")
                                            .rounded_full()
                                            .icon(gpui_kit::assets::IconName::Clipboard)
                                            .label(if self.clipboard_count > 0 {
                                                format!("粘贴文件 ({})", self.clipboard_count)
                                            } else {
                                                "粘贴文件".into()
                                            })
                                            .disabled(self.busy || self.clipboard_count == 0)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                if let Some(item) = cx.read_from_clipboard() {
                                                    this.import(clipboard_paths(&item), window, cx);
                                                }
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .pt_1()
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child("起始分片")
                                    .child(
                                        Input::new(&self.start).rounded_xl().disabled(self.busy),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child("结束分片")
                                    .child(Input::new(&self.end).rounded_xl().disabled(self.busy)),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .items_center()
                            .flex_wrap()
                            .child(div().mr_1().text_color(rgb(0xb6bfce)).child("输出"))
                            .child(
                                Button::new("mp4")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::Video)
                                    .label("MP4")
                                    .selected(!self.ts)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ts = false;
                                        this.invalidate(cx);
                                    })),
                            )
                            .child(
                                Button::new("ts")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::File)
                                    .label("TS")
                                    .selected(self.ts)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ts = true;
                                        this.invalidate(cx);
                                    })),
                            )
                            .child(div().mx_2().w_px().h_5().bg(rgb(0x303641)))
                            .child(
                                Button::new("recursive")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::Folder)
                                    .label("递归子目录")
                                    .selected(self.recursive)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.recursive = !this.recursive;
                                        this.invalidate(cx);
                                    })),
                            )
                            .child(
                                Button::new("hidden")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::Eye)
                                    .label("包含隐藏文件")
                                    .selected(self.hidden)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.hidden = !this.hidden;
                                        this.invalidate(cx);
                                    })),
                            )
                            .child(
                                Button::new("split")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::Scissors)
                                    .label("缺片处拆分")
                                    .selected(self.split)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.split = !this.split;
                                        this.invalidate(cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .pt_4()
                            .border_t_1()
                            .border_color(rgb(0x303641))
                            .child(
                                div()
                                    .mb_3()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("文件预览"),
                            )
                            .child(preview),
                    ),
            )
            .child(
                div()
                    .px_6()
                    .py_4()
                    .border_t_1()
                    .border_color(rgb(0x303641))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .bg(rgb(0x1b2028))
                    .child(
                        div()
                            .text_color(if self.error {
                                rgb(0xffaaa5)
                            } else {
                                rgb(0xb6bfce)
                            })
                            .child(self.message.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("reset")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::RotateCcw)
                                    .label("重选")
                                    .disabled(
                                        self.busy
                                            || self.imported.is_empty()
                                                && self.path.read(cx).value().is_empty(),
                                    )
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.path.update(cx, |input, cx| {
                                            input.set_value("", window, cx)
                                        });
                                        this.imported.clear();
                                        this.invalidate(cx);
                                    })),
                            )
                            .child(
                                Button::new("detect")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::Search)
                                    .label(if self.busy {
                                        "处理中…"
                                    } else {
                                        "检测文件"
                                    })
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.detect(cx))),
                            )
                            .child(
                                Button::new("merge")
                                    .rounded_full()
                                    .icon(gpui_kit::assets::IconName::Play)
                                    .primary()
                                    .label("确认合并")
                                    .disabled(self.busy || self.plan.is_none())
                                    .on_click(cx.listener(|this, _, _, cx| this.merge(cx))),
                            ),
                    ),
            )
    }
}

#[cfg(target_os = "macos")]
fn install_application_icon() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const ICON: &[u8] = include_bytes!("../assets/icon.png");
    let Some(market) = MainThreadMarker::new() else {
        return;
    };
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(ICON));
    if let Some(image) = image {
        unsafe {
            NSApplication::sharedApplication(market).setApplicationIconImage(Some(&image));
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn install_application_icon() {}
pub fn run() -> anyhow::Result<()> {
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            install_application_icon();
            Theme::change(ThemeMode::Dark, None, cx);
            let theme = Theme::global_mut(cx);
            theme.primary = rgb(0x87b4fa).into();
            theme.primary_foreground = rgb(0x101722).into();
            theme.primary_hover = rgb(0xa1c5ff).into();
            theme.primary_active = rgb(0x6a9de9).into();
            theme.ring = rgb(0x87b4fa).into();
            theme.button_primary = theme.primary;
            theme.button_primary_foreground = theme.primary_foreground;
            theme.button_primary_hover = theme.primary_hover;
            theme.button_primary_active = theme.primary_active;
            Theme::sync_base(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(860.), px(740.)),
                        cx,
                    ))),
                    window_min_size: Some(size(px(680.), px(560.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("rcm · 分片合并".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| Merger::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("无法打开分片合并窗口");
            cx.activate(true);
        });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_paths;
    use std::path::PathBuf;
    #[test]
    fn file_urls_decode_spaces_and_preserve_selection() {
        assert_eq!(
            parse_paths("file:///tmp/a%20b.ts\n/tmp/c.ts"),
            vec![PathBuf::from("/tmp/a b.ts"), PathBuf::from("/tmp/c.ts")]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn embedded_icon_is_a_1024_square_png() {
        let icon = include_bytes!("../assets/icon.png");
        assert_eq!(&icon[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(icon[16..20].try_into().unwrap()), 1024);
        assert_eq!(u32::from_be_bytes(icon[20..24].try_into().unwrap()), 1024);
    }
}
