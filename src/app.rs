//! The mockterm application: owns the tabs (each a pane tree), the terminal
//! backends, and the eframe update loop that renders the active tab's panes,
//! handles draggable dividers, routes keyboard shortcuts, and drains PTY events.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui;
use egui::{
    Color32, CornerRadius, CursorIcon, FontId, Id, Key, KeyboardShortcut, Margin,
    Modifiers, Rect, Sense, Stroke, StrokeKind, UiBuilder,
};
use egui_term::{
    BackendCommand, BackendSettings, FontSettings, PtyEvent, TerminalBackend,
    TerminalFont, TerminalTheme, TerminalView,
};

use crate::layout::{neighbor, Axis, Dir, PaneId, Tree};

/// Launch configuration (what each pane runs).
pub struct Config {
    pub shell: String,
    pub args: Vec<String>,
}

struct Pane {
    backend: TerminalBackend,
    title: String,
}

/// One tab: an independent pane layout with its own focused pane.
struct Tab {
    tree: Tree,
    focused: PaneId,
    /// Content rect of the last frame this tab was drawn, for spatial navigation.
    last_area: Rect,
}

pub struct MockTerm {
    tabs: Vec<Tab>,
    active: usize,
    /// All panes across all tabs, keyed by their globally-unique id.
    panes: HashMap<PaneId, Pane>,
    next_id: u64,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    theme: TerminalTheme,
    font: TerminalFont,
    cfg: Config,
    default_title: String,
}

const ACCENT: Color32 = Color32::from_rgb(102, 161, 255);
const DIV_IDLE: Color32 = Color32::from_rgb(38, 40, 48);
const DIV_HOT: Color32 = Color32::from_rgb(90, 120, 180);

impl MockTerm {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        let (pty_tx, pty_rx) = channel();
        let default_title = shell_basename(&cfg.shell);
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            panes: HashMap::new(),
            next_id: 0,
            pty_tx,
            pty_rx,
            theme: TerminalTheme::default(),
            font: TerminalFont::new(FontSettings {
                font_type: FontId::monospace(14.0),
            }),
            cfg,
            default_title,
        };
        // First tab fills the window. If the shell can't spawn we can't do
        // anything useful, so fail loudly.
        let id = app
            .spawn_pane(&cc.egui_ctx)
            .expect("failed to spawn initial shell");
        app.tabs.push(Tab {
            tree: Tree::new(id),
            focused: id,
            last_area: Rect::ZERO,
        });
        app.active = 0;
        app
    }

    /// Spawn a fresh terminal backend and register it in the global pane map.
    fn spawn_pane(&mut self, ctx: &egui::Context) -> io::Result<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let backend = TerminalBackend::new(
            id,
            ctx.clone(),
            self.pty_tx.clone(),
            BackendSettings {
                shell: self.cfg.shell.clone(),
                args: self.cfg.args.clone(),
                working_directory: None,
            },
        )?;
        self.panes.insert(
            id,
            Pane {
                backend,
                title: self.default_title.clone(),
            },
        );
        Ok(id)
    }

    /// Open a new tab containing a single fresh pane, and focus it.
    fn new_tab(&mut self, ctx: &egui::Context) {
        match self.spawn_pane(ctx) {
            Ok(id) => {
                self.tabs.push(Tab {
                    tree: Tree::new(id),
                    focused: id,
                    last_area: Rect::ZERO,
                });
                self.active = self.tabs.len() - 1;
            }
            Err(e) => eprintln!("mockterm: failed to open tab: {e}"),
        }
    }

    /// Split the active tab's focused pane along `axis`.
    fn split(&mut self, axis: Axis, ctx: &egui::Context) {
        match self.spawn_pane(ctx) {
            Ok(id) => {
                let tab = &mut self.tabs[self.active];
                tab.tree.split(tab.focused, id, axis, true);
                tab.focused = id;
            }
            Err(e) => eprintln!("mockterm: failed to spawn pane: {e}"),
        }
    }

    fn close_pane(&mut self, pane: PaneId, ctx: &egui::Context) {
        if !self.panes.contains_key(&pane) {
            return; // already gone (e.g. duplicate Exit + ChildExit)
        }
        let Some(ti) = self.tabs.iter().position(|t| t.tree.contains(pane)) else {
            self.panes.remove(&pane);
            return;
        };

        let next = self.tabs[ti].tree.focus_after_close(pane);
        let removed = self.tabs[ti].tree.close(pane);
        // Dropping the backend sends Shutdown to its PTY loop, killing the shell.
        self.panes.remove(&pane);

        if !removed {
            // That was the tab's last pane — drop the whole tab.
            self.tabs.remove(ti);
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if self.active > ti {
                self.active -= 1;
            }
        } else if self.tabs[ti].focused == pane {
            let fallback = self.tabs[ti].tree.first_pane();
            let nf = next
                .filter(|p| self.panes.contains_key(p))
                .unwrap_or(fallback);
            self.tabs[ti].focused = nf;
        }
    }

    /// Pull terminal output / control events off the PTY channel.
    fn drain_pty_events(&mut self, ctx: &egui::Context) {
        let mut to_close = Vec::new();
        while let Ok((id, event)) = self.pty_rx.try_recv() {
            match event {
                PtyEvent::Title(t) => {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.title = t;
                    }
                }
                PtyEvent::ResetTitle => {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.title = self.default_title.clone();
                    }
                }
                PtyEvent::PtyWrite(text) => {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.backend
                            .process_command(BackendCommand::Write(text.into_bytes()));
                    }
                }
                PtyEvent::Exit | PtyEvent::ChildExit(_) => to_close.push(id),
                _ => {}
            }
        }
        for id in to_close {
            self.close_pane(id, ctx);
        }
    }

    /// Intercept multiplexer shortcuts before terminals see the key events.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let cmd_alt = Modifiers::COMMAND | Modifiers::ALT;

        let hit = |mods: Modifiers, key: Key| -> bool {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(mods, key)))
        };

        // Tabs: new tab, and jump to tab N by number.
        if hit(cmd, Key::T) {
            self.new_tab(ctx);
        }
        const NUM_KEYS: [Key; 9] = [
            Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6,
            Key::Num7, Key::Num8, Key::Num9,
        ];
        for (i, key) in NUM_KEYS.iter().enumerate() {
            if hit(cmd, *key) && i < self.tabs.len() {
                self.active = i;
            }
        }

        // Splits.
        if hit(cmd, Key::D) {
            self.split(Axis::Horizontal, ctx);
        }
        if hit(cmd_shift, Key::D) {
            self.split(Axis::Vertical, ctx);
        }
        // Close focused pane (collapses/removes its tab when it was the last).
        if hit(cmd, Key::W) {
            let pane = self.tabs[self.active].focused;
            self.close_pane(pane, ctx);
        }

        // Directional navigation within the active tab.
        let nav = [
            (Key::ArrowLeft, Dir::Left),
            (Key::ArrowRight, Dir::Right),
            (Key::ArrowUp, Dir::Up),
            (Key::ArrowDown, Dir::Down),
        ];
        for (key, dir) in nav {
            if hit(cmd_alt, key) {
                let a = self.active;
                let (leaves, _) = self.tabs[a].tree.geometry(self.tabs[a].last_area);
                if let Some(p) = neighbor(&leaves, self.tabs[a].focused, dir) {
                    self.tabs[a].focused = p;
                }
            }
        }
    }

    fn draw_tab_strip(&mut self, ctx: &egui::Context) {
        let mut switch_to: Option<usize> = None;
        let mut open_new = false;
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for (i, tab) in self.tabs.iter().enumerate() {
                    let raw = self
                        .panes
                        .get(&tab.focused)
                        .map(|p| p.title.as_str())
                        .unwrap_or("—");
                    let label = format!("{}  {}", i + 1, truncate(raw, 18));
                    if ui.selectable_label(i == self.active, label).clicked() {
                        switch_to = Some(i);
                    }
                }
                if ui.button("+").on_hover_text("New tab (Cmd+T)").clicked() {
                    open_new = true;
                }
            });
        });
        if let Some(i) = switch_to {
            self.active = i;
        }
        if open_new {
            self.new_tab(ctx);
        }
    }
}

impl eframe::App for MockTerm {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_pty_events(ctx);
        self.handle_shortcuts(ctx);
        self.draw_tab_strip(ctx);

        // Status / hint bar (active tab's focused pane + shortcut hints).
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let title = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| self.panes.get(&t.focused))
                    .map(|p| p.title.as_str())
                    .unwrap_or("mockterm");
                ui.label(
                    egui::RichText::new(format!("▌ {title}"))
                        .color(ACCENT)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            "⌘T tab  ⌘1-9 switch  ⌘D split→  ⌘⇧D split↓  ⌘W close  ⌘⌥←→↑↓ move  drag borders",
                        )
                        .color(Color32::from_gray(120))
                        .size(12.0),
                    );
                });
            });
        });

        let active = self.active;
        let focused = self.tabs[active].focused;
        let theme = self.theme.clone();
        let font = self.font.clone();

        let frame = egui::Frame::default()
            .fill(Color32::from_rgb(16, 17, 21))
            .inner_margin(Margin::ZERO);

        let mut clicked: Option<PaneId> = None;
        let mut ratio_updates: Vec<(usize, f32)> = Vec::new();

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let area = ui.max_rect();
            self.tabs[active].last_area = area;
            let (leaves, dividers) = self.tabs[active].tree.geometry(area);

            // 1) Draw each pane's terminal.
            for (pane_id, rect) in &leaves {
                let Some(pane) = self.panes.get_mut(pane_id) else {
                    continue;
                };
                let resp = ui
                    .allocate_new_ui(UiBuilder::new().max_rect(*rect), |ui| {
                        let view = TerminalView::new(ui, &mut pane.backend)
                            .set_focus(*pane_id == focused)
                            .set_theme(theme.clone())
                            .set_font(font.clone())
                            .set_size(rect.size());
                        ui.add(view)
                    })
                    .inner;
                if resp.clicked() {
                    clicked = Some(*pane_id);
                }
            }

            // 2) Draggable dividers on top of the panes.
            for div in &dividers {
                let id = Id::new(("mockterm_divider", active, div.node));
                let resp = ui.interact(div.rect, id, Sense::drag());
                let hot = resp.hovered() || resp.dragged();
                if hot {
                    ctx.set_cursor_icon(match div.axis {
                        Axis::Horizontal => CursorIcon::ResizeHorizontal,
                        Axis::Vertical => CursorIcon::ResizeVertical,
                    });
                }
                ui.painter().rect_filled(
                    div.rect,
                    CornerRadius::ZERO,
                    if hot { DIV_HOT } else { DIV_IDLE },
                );
                if resp.dragged() && div.avail > 1.0 {
                    let delta = resp.drag_delta();
                    let along = match div.axis {
                        Axis::Horizontal => delta.x,
                        Axis::Vertical => delta.y,
                    };
                    ratio_updates.push((div.node, div.ratio + along / div.avail));
                }
            }

            // 3) Accent border around the focused pane, painted last (on top).
            if let Some((_, rect)) = leaves.iter().find(|(p, _)| *p == focused) {
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    CornerRadius::ZERO,
                    Stroke::new(1.5, ACCENT),
                    StrokeKind::Inside,
                );
            }
        });

        if let Some(p) = clicked {
            self.tabs[active].focused = p;
        }
        for (node, ratio) in ratio_updates {
            self.tabs[active].tree.set_ratio(node, ratio);
        }
    }
}

fn shell_basename(shell: &str) -> String {
    shell
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(shell)
        .to_string()
}

/// Shorten a title for the tab strip, appending an ellipsis when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}
