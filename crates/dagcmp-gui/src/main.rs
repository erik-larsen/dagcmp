#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use std::path::PathBuf;
use std::sync::mpsc;
use dagcmp_core::compare::{compare, Counts, DiffNode, Status};
use dagcmp_core::model::Meta;
use dagcmp_core::scan::{
    scan_pair_with_progress, ScanMethod, ScanOptions, ScanProgress, ScanResult,
};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "dagcmp",
        options,
        Box::new(|_cc| {
            Ok(Box::new(App {
                use_mft: true,
                ..Default::default()
            }))
        }),
    )
}

struct ScanOutcome {
    left: ScanResult,
    right: ScanResult,
    diff: DiffNode,
}

#[derive(Default)]
struct App {
    left_path: String,
    right_path: String,
    use_mft: bool,
    scanning: bool,
    show_identical: bool,
    status_line: String,
    outcome: Option<ScanOutcome>,
    /// Path (component names) of the directory selected in the tree pane.
    selected: Vec<String>,
    rx: Option<mpsc::Receiver<Result<ScanOutcome, String>>>,
    progress_rx: Option<mpsc::Receiver<String>>,
}

fn progress_text(side: &str, p: ScanProgress) -> String {
    match p {
        ScanProgress::MftLoading => format!("{side}: reading MFT…"),
        ScanProgress::MftReading { bytes } => {
            format!("{side}: reading MFT… {}", human_bytes(bytes))
        }
        ScanProgress::MftLoaded { records, bytes } => {
            format!("{side}: MFT loaded ({records} records, {})", human_bytes(bytes))
        }
        ScanProgress::MftRecords {
            done,
            total,
            files,
            dirs,
        } => {
            format!("{side}: MFT records {done}/{total} — {files} files, {dirs} dirs matched")
        }
        ScanProgress::WalkEntries { entries } => format!("{side}: {entries} entries walked"),
    }
}

impl App {
    fn start_scan(&mut self) {
        let left = PathBuf::from(self.left_path.trim());
        let right = PathBuf::from(self.right_path.trim());
        if !left.is_dir() || !right.is_dir() {
            self.status_line = "Both paths must be existing directories.".into();
            return;
        }
        let options = ScanOptions {
            try_mft: self.use_mft,
        };
        let (tx, rx) = mpsc::channel();
        let (ptx, prx) = mpsc::channel();
        self.rx = Some(rx);
        self.progress_rx = Some(prx);
        self.scanning = true;
        self.status_line = "Scanning…".into();
        std::thread::spawn(move || {
            let result = (|| {
                let (l, r) = scan_pair_with_progress(&left, &right, options, &mut |p| {
                    let _ = ptx.send(progress_text("scan", p));
                })
                .map_err(|e| e.to_string())?;
                let _ = ptx.send("comparing…".to_string());
                let diff = compare(&l.root, &r.root);
                Ok(ScanOutcome {
                    left: l,
                    right: r,
                    diff,
                })
            })();
            let _ = tx.send(result);
        });
    }

    fn poll_scan(&mut self) {
        if let Some(prx) = &self.progress_rx {
            let mut latest = None;
            while let Ok(msg) = prx.try_recv() {
                latest = Some(msg);
            }
            if let Some(msg) = latest {
                if self.scanning {
                    self.status_line = msg;
                }
            }
        }
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(Ok(outcome)) => {
                    self.status_line = format!(
                        "Left: {} via {}   |   Right: {} via {}",
                        format_scan(&outcome.left),
                        method_name(&outcome.left),
                        format_scan(&outcome.right),
                        method_name(&outcome.right),
                    );
                    self.outcome = Some(outcome);
                    self.selected.clear();
                    self.scanning = false;
                    self.rx = None;
                    self.progress_rx = None;
                }
                Ok(Err(msg)) => {
                    self.status_line = format!("Scan failed: {msg}");
                    self.scanning = false;
                    self.rx = None;
                    self.progress_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status_line = "Scan thread died unexpectedly.".into();
                    self.scanning = false;
                    self.rx = None;
                    self.progress_rx = None;
                }
            }
        }
    }
}

fn format_scan(r: &ScanResult) -> String {
    let (files, dirs, _) = r.root.totals();
    format!("{files} files / {dirs} dirs in {:.2?}", r.duration)
}

fn method_name(r: &ScanResult) -> &'static str {
    match r.method {
        ScanMethod::Mft => "MFT",
        ScanMethod::Walk => "walk",
    }
}

fn status_color(s: Status) -> egui::Color32 {
    match s {
        Status::Identical => egui::Color32::GRAY,
        Status::LeftNewer | Status::LeftOnly => egui::Color32::from_rgb(70, 130, 255),
        Status::RightNewer | Status::RightOnly => egui::Color32::from_rgb(230, 80, 80),
        Status::Different => egui::Color32::from_rgb(200, 90, 220),
    }
}

fn status_label(s: Status) -> &'static str {
    match s {
        Status::Identical => "identical",
        Status::LeftNewer => "left is newer",
        Status::RightNewer => "right is newer",
        Status::Different => "different (same age, different size)",
        Status::LeftOnly => "left side only",
        Status::RightOnly => "right side only",
    }
}

/// Set attribute letters only, TreeComp-style ("A", "RH", …).
fn attr_string(meta: Option<Meta>) -> String {
    let Some(m) = meta else {
        return String::new();
    };
    let mut s = String::new();
    for (bit, ch) in [(0x1, 'R'), (0x2, 'H'), (0x4, 'S'), (0x20, 'A')] {
        if m.attrs & bit != 0 {
            s.push(ch);
        }
    }
    s
}

fn fmt_size(is_dir: bool, meta: Option<Meta>) -> String {
    match meta {
        Some(m) if !is_dir => human_bytes(m.size),
        _ => String::new(),
    }
}

fn fmt_time(meta: Option<Meta>) -> String {
    let Some(t) = meta.and_then(|m| m.mtime_systemtime()) else {
        return String::new();
    };
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y%m%d %H:%M:%S").to_string()
}

/// Solid painted status square (font glyphs like ● render as tofu boxes in
/// egui's default fonts).
fn status_swatch(ui: &mut egui::Ui, color: egui::Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect.shrink(0.5), 2.0, color);
    resp
}

/// Aggregate color for a directory: gray when the subtree is in sync, blue
/// when all differences favor the left side, red when all favor the right,
/// purple when mixed.
fn dir_status_color(c: &Counts) -> egui::Color32 {
    let left = c.left_only + c.left_newer;
    let right = c.right_only + c.right_newer;
    if c.in_sync() {
        egui::Color32::GRAY
    } else if c.different == 0 && right == 0 {
        status_color(Status::LeftOnly)
    } else if c.different == 0 && left == 0 {
        status_color(Status::RightOnly)
    } else {
        status_color(Status::Different)
    }
}

/// Resolve a tree-pane selection path to its node.
fn find_node<'a>(root: &'a DiffNode, path: &[String]) -> Option<&'a DiffNode> {
    let mut node = root;
    for name in path {
        node = node
            .children
            .iter()
            .find(|c| c.is_dir && c.name == *name)?;
    }
    Some(node)
}

/// Directory tree pane (directories only), File Explorer style.
fn dir_tree(
    ui: &mut egui::Ui,
    node: &DiffNode,
    path: &mut Vec<String>,
    selected: &[String],
    nav: &mut Option<Vec<String>>,
    show_identical: bool,
) {
    for child in &node.children {
        if !child.is_dir {
            continue;
        }
        if !show_identical && child.counts.in_sync() {
            continue;
        }
        path.push(child.name.clone());
        let has_subdirs = child.children.iter().any(|c| {
            c.is_dir && (show_identical || !c.counts.in_sync())
        });
        let is_selected = selected == path.as_slice();
        let swatch_color = dir_status_color(&child.counts);
        let text = egui::RichText::new(&child.name);
        if has_subdirs {
            let id = ui.make_persistent_id(&path);
            egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                id,
                path.len() <= 1,
            )
            .show_header(ui, |ui| {
                status_swatch(ui, swatch_color);
                if ui.selectable_label(is_selected, text).clicked() {
                    *nav = Some(path.clone());
                }
            })
            .body(|ui| {
                dir_tree(ui, child, path, selected, nav, show_identical);
            });
        } else {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                status_swatch(ui, swatch_color);
                if ui.selectable_label(is_selected, text).clicked() {
                    *nav = Some(path.clone());
                }
            });
        }
        path.pop();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();
        if self.scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("paths").show(ctx, |ui| {
            ui.add_space(6.0);
            path_row(ui, "Left:", &mut self.left_path);
            path_row(ui, "Right:", &mut self.right_path);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.scanning, egui::Button::new("Compare"))
                    .clicked()
                {
                    self.start_scan();
                }
                ui.checkbox(&mut self.use_mft, "Use MFT fast path (needs elevation)");
                ui.checkbox(&mut self.show_identical, "Show identical");
                if self.scanning {
                    ui.spinner();
                }
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(&self.status_line);
            if let Some(o) = &self.outcome {
                ui.label(summary_text(&o.diff.counts));
            }
            ui.add_space(4.0);
        });

        // Selection changes requested by either pane, applied after drawing.
        let mut nav: Option<Vec<String>> = None;
        let selected = self.selected.clone();
        let show_identical = self.show_identical;

        egui::SidePanel::left("tree_pane")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    if let Some(o) = &self.outcome {
                        ui.horizontal(|ui| {
                            status_swatch(ui, dir_status_color(&o.diff.counts));
                            if ui
                                .selectable_label(selected.is_empty(), "Root")
                                .clicked()
                            {
                                nav = Some(Vec::new());
                            }
                        });
                        let mut path = Vec::new();
                        dir_tree(ui, &o.diff, &mut path, &selected, &mut nav, show_identical);
                    } else if !self.scanning {
                        ui.weak("No comparison yet.");
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(o) = &self.outcome else {
                if !self.scanning {
                    ui.weak("Pick two folders and press Compare.");
                }
                return;
            };
            let node = find_node(&o.diff, &selected).unwrap_or(&o.diff);

            // Root paths, TreeComp-style: left in blue, right in red.
            ui.horizontal(|ui| {
                ui.colored_label(
                    status_color(Status::LeftOnly),
                    o.left.root_path.display().to_string(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(
                        status_color(Status::RightOnly),
                        o.right.root_path.display().to_string(),
                    );
                });
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!selected.is_empty(), egui::Button::new("⬆ Up"))
                    .clicked()
                {
                    let mut p = selected.clone();
                    p.pop();
                    nav = Some(p);
                }
                ui.label(if selected.is_empty() {
                    "\\".to_string()
                } else {
                    format!("\\{}", selected.join("\\"))
                });
            });
            ui.separator();

            let row_h = 20.0;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto().at_least(46.0)) // status
                .column(Column::auto().at_least(56.0)) // attr L
                .column(Column::auto().at_least(76.0)) // size L
                .column(Column::auto().at_least(126.0)) // modified L
                .column(Column::remainder().at_least(160.0)) // name
                .column(Column::auto().at_least(126.0)) // modified R
                .column(Column::auto().at_least(76.0)) // size R
                .column(Column::auto().at_least(56.0)) // attr R
                .header(22.0, |mut header| {
                    for title in [
                        "Status",
                        "Attrib Left",
                        "Size Left",
                        "Modified Left",
                        "Name",
                        "Modified Right",
                        "Size Right",
                        "Attrib Right",
                    ] {
                        header.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|mut body| {
                    for child in &node.children {
                        if child.is_dir {
                            continue; // folders live in the tree pane only
                        }
                        if !show_identical && child.counts.in_sync() {
                            continue;
                        }
                        body.row(row_h, |mut row| {
                            row.col(|ui| {
                                status_swatch(ui, status_color(child.status))
                                    .on_hover_text(status_label(child.status));
                            });
                            row.col(|ui| {
                                ui.monospace(attr_string(child.left));
                            });
                            row.col(|ui| {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(fmt_size(child.is_dir, child.left));
                                    },
                                );
                            });
                            row.col(|ui| {
                                ui.label(fmt_time(child.left));
                            });
                            row.col(|ui| {
                                ui.label(
                                    egui::RichText::new(&child.name)
                                        .color(status_color(child.status)),
                                )
                                .on_hover_text(status_label(child.status));
                            });
                            row.col(|ui| {
                                ui.label(fmt_time(child.right));
                            });
                            row.col(|ui| {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(fmt_size(child.is_dir, child.right));
                                    },
                                );
                            });
                            row.col(|ui| {
                                ui.monospace(attr_string(child.right));
                            });
                        });
                    }
                });
        });

        if let Some(p) = nav {
            self.selected = p;
        }
    }
}

/// One "Label: [ text field stretching to fill ] [Browse…]" row.
fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        // Fixed label width so both rows' fields start at the same x.
        ui.add_sized([44.0, 18.0], egui::Label::new(label));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let browse_clicked = ui.button("Browse…").clicked();
            ui.add(egui::TextEdit::singleline(value).desired_width(ui.available_width()));
            if browse_clicked {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    *value = dir.display().to_string();
                }
            }
        });
    });
}

fn summary_text(c: &Counts) -> String {
    format!(
        "identical {}  |  ◀ newer {}  |  newer ▶ {}  |  ≠ {}  |  ◀ only {}  |  only ▶ {}",
        c.identical, c.left_newer, c.right_newer, c.different, c.left_only, c.right_only
    )
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}
