//! Accessibility: the whole window as an AccessKit tree, published over
//! AT-SPI by accesskit_unix.
//!
//! Window
//!  ├─ TabList ─ Tab × n           (when there is more than one tab)
//!  ├─ Terminal (active tab only) ─ TextRun per screen line
//!  ├─ Menu ─ MenuItem × n          (context menu, when open)
//!  ├─ Dialog "Settings" ─ CheckBox/SpinButton/ComboBox/TextInput
//!  ├─ SearchInput "Find"           (when open)
//!  └─ Status (live region)         (announcements: "Copied", "Match 3 of 12")
//!
//! Terminal lines are sent incrementally (only changed lines); AccessKit
//! diffs the Terminal's text and emits AT-SPI text-changed / caret-moved
//! events. Each tab's Terminal has its own node ids, so switching tabs is a
//! focus change (announced as such) rather than a giant text replacement.

use crate::config::Config;
use crate::term::{Term, flags};
use crate::ui::{self, Kind, Overlay};
use accesskit::{
    Action, Live, Node, NodeId, Rect, Role, TextDirection, TextPosition, TextSelection, Toggled, TreeId,
    TreeInfo, TreeUpdate,
};

pub const ROOT: NodeId = NodeId(1);
const TABLIST: NodeId = NodeId(3);
const MENU: NodeId = NodeId(4);
const SETTINGS: NodeId = NodeId(5);
const SEARCH: NodeId = NodeId(6);
const STATUS: NodeId = NodeId(7);
const SEARCH_TEXT: NodeId = NodeId(8);
pub const TAB_BASE: u64 = 100;
pub const MENU_ITEM_BASE: u64 = 200;
const SETTING_BASE: u64 = 300;
/// TextRun child of the settings text field at index i (one per field:
/// AccessKit rejects a tree where two nodes share a child id).
const SETTING_TEXT_BASE: u64 = 400;
/// Read-only TextInput child of the settings combo box at index i.
const SETTING_COMBO_TEXT_BASE: u64 = 500;

pub fn term_node(tab_id: u64) -> NodeId {
    NodeId((tab_id + 1) << 32)
}
fn line_node(tab_id: u64, y: usize) -> NodeId {
    NodeId(((tab_id + 1) << 32) + 1 + y as u64)
}

pub struct Geometry {
    pub cell_w: f64,
    pub cell_h: f64,
    pub pad: f64,
    /// Rows of window chrome above the terminal (tab bar).
    pub bar_rows: usize,
    /// Window grid size in cells (terminal + chrome).
    pub cols: usize,
    pub rows: usize,
}

impl Geometry {
    fn cells(&self, (x, y, w, h): ui::CellRect) -> Rect {
        Rect {
            x0: self.pad + x as f64 * self.cell_w,
            y0: self.pad + y as f64 * self.cell_h,
            x1: self.pad + (x + w) as f64 * self.cell_w,
            y1: self.pad + (y + h) as f64 * self.cell_h,
        }
    }
}

/// Everything outside the terminal that the tree describes.
pub struct UiView<'a> {
    pub title: &'a str,
    pub tab_id: u64,
    /// (title, active) for every tab.
    pub tabs: &'a [(String, bool)],
    pub overlay: &'a Overlay,
    pub cfg: &'a Config,
    pub announcement: &'a str,
    /// Running as the dropdown (quake-style) window.
    pub dropdown: bool,
}

#[derive(Default)]
pub struct A11y {
    prev_lines: Vec<String>,
    /// The TextRun node last sent for each line (re-sent shifted on scroll).
    prev_nodes: Vec<Node>,
    /// Lines `..valid` of `prev_lines` are in the tree; the rest must be sent.
    valid: usize,
    prev_tab: Option<u64>,
    /// (alt screen, absolute line of the top row) at the last update.
    prev_top: Option<(bool, i64)>,
    prev_caret: Option<(usize, usize)>,
    prev_focus: Option<NodeId>,
    sent_tree: bool,
}

struct Line {
    text: String,
    lens: Vec<u8>,
    pos: Vec<f32>,
    widths: Vec<f32>,
    cols: Vec<usize>,
}

fn extract_line(term: &Term, y: usize, cursor_col: Option<usize>, last: bool, cw: f32) -> Line {
    let grid = term.screen();
    let row = grid.view_row(y, term.display_offset);
    // Trim trailing blanks, but keep everything up to the cursor.
    let mut end = row.cells.iter().rposition(|c| c.c != ' ').map_or(0, |i| i + 1);
    if let Some(cx) = cursor_col {
        end = end.max(cx.min(grid.cols));
    }
    let mut l = Line {
        text: String::with_capacity(end + 1),
        lens: Vec::with_capacity(end + 1),
        pos: Vec::with_capacity(end + 1),
        widths: Vec::with_capacity(end + 1),
        cols: Vec::with_capacity(end + 1),
    };
    for (x, cell) in row.cells[..end].iter().enumerate() {
        if cell.flags & flags::WIDE_SPACER != 0 {
            continue;
        }
        let c = if cell.flags & flags::HIDDEN != 0 { ' ' } else { cell.c };
        l.text.push(c);
        l.lens.push(c.len_utf8() as u8);
        l.pos.push(x as f32 * cw);
        l.widths.push(if cell.flags & flags::WIDE != 0 { cw * 2.0 } else { cw });
        l.cols.push(x);
    }
    if !last && !row.wrapped {
        l.text.push('\n');
        l.lens.push(1);
        l.pos.push(end as f32 * cw);
        l.widths.push(0.0);
        l.cols.push(end);
    }
    l
}

/// The Terminal node (without children or selection) showing `rows` lines.
fn terminal_node(term: &Term, g: &Geometry, title: &str, rows: usize) -> Node {
    let mut tn = Node::new(Role::Terminal);
    tn.set_label(title.to_string());
    tn.set_bounds(g.cells((0, g.bar_rows, term.cols(), rows)));
    tn.add_action(Action::Focus);
    tn.add_action(Action::ShowContextMenu);
    tn
}

/// Give a single-line text field its text: AT-SPI's Text interface (what
/// screen readers read and track the caret in) comes from a TextRun child.
fn text_field(field: &mut Node, run_id: NodeId, text: &str, bounds: Rect, cell_w: f64) -> (NodeId, Node) {
    let mut run = Node::new(Role::TextRun);
    run.set_value(text.to_string());
    run.set_character_lengths(text.chars().map(|c| c.len_utf8() as u8).collect::<Vec<_>>());
    run.set_character_positions((0..text.chars().count()).map(|i| (i as f64 * cell_w) as f32).collect::<Vec<_>>());
    run.set_character_widths(vec![cell_w as f32; text.chars().count()]);
    run.set_text_direction(TextDirection::LeftToRight);
    run.set_bounds(bounds);
    if let Some(ws) = word_starts(text) {
        run.set_word_starts(ws);
    }
    field.set_children(vec![run_id]);
    let end = TextPosition { node: run_id, character_index: text.chars().count() };
    field.set_text_selection(TextSelection { anchor: end, focus: end });
    (run_id, run)
}

fn word_starts(text: &str) -> Option<Vec<u8>> {
    let mut starts = Vec::new();
    let mut prev_space = true;
    for (i, c) in text.chars().enumerate() {
        let space = c.is_whitespace();
        if !space && prev_space {
            starts.push(u8::try_from(i).ok()?);
        }
        prev_space = space;
    }
    Some(starts)
}

impl A11y {
    /// Forget what was sent so the next update is a full tree.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// When the screen scrolled up by k lines since the last update, an
    /// update that only drops the top k lines. Sent before `update`, it turns
    /// a scroll into "k lines removed at the start" plus "new lines appended
    /// at the end". In one update AccessKit's prefix/suffix text diff sees
    /// every line change and reports the whole screen as removed and
    /// re-inserted, so Orca re-reads the entire screen on every scroll and
    /// falls further and further behind the output.
    pub fn scroll(&mut self, term: &Term, g: &Geometry, ui: &UiView<'_>) -> Option<TreeUpdate> {
        let top = (term.alt_active, term.abs_line(0));
        let prev = self.prev_top?;
        let rows = term.rows();
        let tid = ui.tab_id;
        if !self.sent_tree || self.prev_tab != Some(tid) || self.prev_lines.len() != rows || prev.0 != top.0 {
            return None;
        }
        let k = usize::try_from(top.1 - prev.1).ok().filter(|&k| k > 0 && k < self.valid)?;
        let focus = self.prev_focus?;
        self.prev_top = Some(top);
        let keep = self.valid - k;
        self.prev_lines.drain(..k);
        self.prev_nodes.drain(..k);
        self.prev_lines.resize(rows, String::new());
        self.prev_nodes.resize(rows, Node::new(Role::TextRun));
        self.valid = keep;

        let mut nodes = Vec::with_capacity(keep + 1);
        for (y, n) in self.prev_nodes[..keep].iter_mut().enumerate() {
            n.set_bounds(g.cells((0, g.bar_rows + y, term.cols(), 1)));
            nodes.push((line_node(tid, y), n.clone()));
        }
        let mut tn = terminal_node(term, g, ui.title, keep);
        tn.set_children((0..keep).map(|y| line_node(tid, y)).collect::<Vec<_>>());
        let caret = match self.prev_caret {
            Some((y, idx)) if y >= k && y - k < keep => (y - k, idx),
            _ => (0, 0),
        };
        let p = TextPosition { node: line_node(tid, caret.0), character_index: caret.1 };
        tn.set_text_selection(TextSelection { anchor: p, focus: p });
        nodes.push((term_node(tid), tn));
        Some(TreeUpdate { nodes, tree: None, tree_id: TreeId::ROOT, focus })
    }

    pub fn update(&mut self, term: &Term, g: &Geometry, ui: &UiView<'_>) -> TreeUpdate {
        let mut nodes = Vec::new();
        let tid = ui.tab_id;
        let term_id = term_node(tid);

        // ---- terminal lines (incremental) ----
        let rows = term.rows();
        let full_term = self.prev_tab != Some(tid) || self.prev_lines.len() != rows;
        if full_term {
            self.prev_lines = vec![String::new(); rows];
            self.prev_nodes = vec![Node::new(Role::TextRun); rows];
            self.valid = 0;
            self.prev_tab = Some(tid);
        }
        let cursor_y = term.cy + term.display_offset;
        let caret_row = (cursor_y < rows).then_some(cursor_y);
        let mut caret = None;
        for y in 0..rows {
            let cursor_col = (caret_row == Some(y)).then_some(term.cx);
            let line = extract_line(term, y, cursor_col, y + 1 == rows, g.cell_w as f32);
            if let Some(cx) = cursor_col {
                let idx = line.cols.iter().position(|&c| c >= cx).unwrap_or(line.cols.len());
                let max = line.text.chars().count() - line.text.ends_with('\n') as usize;
                caret = Some((y, idx.min(max)));
            }
            if y < self.valid && self.prev_lines[y] == line.text {
                continue;
            }
            let mut n = Node::new(Role::TextRun);
            n.set_bounds(g.cells((0, g.bar_rows + y, term.cols(), 1)));
            n.set_text_direction(TextDirection::LeftToRight);
            if let Some(ws) = word_starts(&line.text) {
                n.set_word_starts(ws);
            }
            n.set_character_lengths(line.lens);
            n.set_character_positions(line.pos);
            n.set_character_widths(line.widths);
            n.set_value(line.text.clone());
            self.prev_lines[y] = line.text;
            self.prev_nodes[y] = n.clone();
            nodes.push((line_node(tid, y), n));
        }
        self.valid = rows;
        self.prev_top = Some((term.alt_active, term.abs_line(0)));
        self.prev_caret = caret;

        let mut tn = terminal_node(term, g, ui.title, rows);
        tn.set_children((0..rows).map(|y| line_node(tid, y)).collect::<Vec<_>>());
        if let Some(sel) = self.term_selection(term, tid, caret) {
            tn.set_text_selection(sel);
        }
        nodes.push((term_id, tn));

        // ---- window chrome ----
        let mut root_children = Vec::new();
        if ui.tabs.len() > 1 {
            root_children.push(TABLIST);
            let mut list = Node::new(Role::TabList);
            list.set_label("Tabs");
            let lay = ui::tab_bar_layout(g.cols, ui.tabs.len());
            let mut kids = Vec::new();
            for (i, (title, active)) in ui.tabs.iter().enumerate() {
                let id = NodeId(TAB_BASE + i as u64);
                kids.push(id);
                let mut t = Node::new(Role::Tab);
                t.set_label(title.clone());
                t.set_selected(*active);
                t.set_position_in_set(i + 1);
                t.set_size_of_set(ui.tabs.len());
                t.add_action(Action::Click);
                if let (true, Some(&(x, w))) = (g.bar_rows > 0, lay.tabs.get(i)) {
                    t.set_bounds(g.cells((x, 0, w, 1)));
                }
                if *active {
                    t.set_controls(vec![term_id]);
                }
                nodes.push((id, t));
            }
            list.set_children(kids);
            nodes.push((TABLIST, list));
        }
        root_children.push(term_id);

        let mut focus = term_id;
        match ui.overlay {
            Overlay::None => {}
            Overlay::Menu(m) => {
                root_children.push(MENU);
                let rect = ui::menu_rect(m, g.cols, g.rows);
                let mut menu = Node::new(Role::Menu);
                menu.set_label("Terminal menu");
                menu.set_bounds(g.cells(rect));
                let real: Vec<usize> =
                    (0..ui::MENU.len()).filter(|&i| ui::MENU[i].action.is_some()).collect();
                let mut kids = Vec::new();
                for (pos, &i) in real.iter().enumerate() {
                    let it = &ui::MENU[i];
                    let id = NodeId(MENU_ITEM_BASE + i as u64);
                    kids.push(id);
                    let mut n = Node::new(Role::MenuItem);
                    n.set_label(it.label.trim_end_matches('…').to_string());
                    if let Some(sc) = m.shortcuts.get(i).filter(|s| !s.is_empty()) {
                        n.set_keyboard_shortcut(sc.clone());
                    }
                    n.set_position_in_set(pos + 1);
                    n.set_size_of_set(real.len());
                    n.add_action(Action::Click);
                    let row = i as isize - m.scroll as isize;
                    if row >= 0 && (row as usize) < ui::menu_visible(rect.3) {
                        n.set_bounds(g.cells((rect.0 + 1, rect.1 + 1 + row as usize, rect.2 - 2, 1)));
                    }
                    nodes.push((id, n));
                }
                menu.set_children(kids);
                nodes.push((MENU, menu));
                focus = NodeId(MENU_ITEM_BASE + m.sel as u64);
            }
            Overlay::Settings(s) => {
                root_children.push(SETTINGS);
                let rect = ui::settings_rect(g.cols, g.rows);
                let mut d = Node::new(Role::Dialog);
                d.set_label("Settings");
                d.set_modal();
                d.set_description(
                    "Up and Down choose a setting, Left and Right change it, Enter toggles or edits, Escape closes. Changes are saved immediately.",
                );
                d.set_bounds(g.cells(rect));
                let mut kids = Vec::new();
                let lay = ui::settings_layout(rect.3);
                let vis = lay.vis;
                for (i, it) in s.items.iter().enumerate() {
                    let id = NodeId(SETTING_BASE + i as u64);
                    let row = i as isize - s.scroll as isize;
                    let row_rect = (row >= 0 && (row as usize) < vis)
                        .then(|| g.cells((rect.0 + 1, rect.1 + lay.top + row as usize, rect.2 - 2, 1)));
                    kids.push(id);
                    let value = ui.cfg.get(it.key);
                    let mut n = match &it.kind {
                        Kind::Bool => {
                            let mut n = Node::new(Role::CheckBox);
                            n.set_toggled(if value == "true" { Toggled::True } else { Toggled::False });
                            n
                        }
                        Kind::Number { min, max, step } => {
                            let mut n = Node::new(Role::SpinButton);
                            n.set_numeric_value(value.parse().unwrap_or(0.0));
                            n.set_min_numeric_value(*min);
                            n.set_max_numeric_value(*max);
                            n.set_numeric_value_step(*step);
                            n.set_value(value.clone());
                            n
                        }
                        Kind::Choice(_) => {
                            // Orca speaks a combo box's value from the text of
                            // its single text-input child (as with GTK entry
                            // combos); AccessKit gives ComboBox itself no Text
                            // interface, so a bare set_value is never spoken.
                            let mut n = Node::new(Role::ComboBox);
                            n.set_value(value.clone());
                            let b = row_rect.unwrap_or_default();
                            let field_id = NodeId(SETTING_COMBO_TEXT_BASE + i as u64);
                            let mut field = Node::new(Role::TextInput);
                            field.set_read_only();
                            field.set_label(it.label.to_string());
                            field.set_value(value.clone());
                            field.set_bounds(b);
                            let run = NodeId(SETTING_TEXT_BASE + i as u64);
                            nodes.push(text_field(&mut field, run, &value, b, g.cell_w));
                            nodes.push((field_id, field));
                            n.set_children(vec![field_id]);
                            n
                        }
                        Kind::Text => {
                            let mut n = Node::new(Role::TextInput);
                            let v = match (&s.editing, i == s.sel) {
                                (Some(e), true) => e.clone(),
                                _ => value.clone(),
                            };
                            let b = row_rect.unwrap_or_default();
                            let run = NodeId(SETTING_TEXT_BASE + i as u64);
                            nodes.push(text_field(&mut n, run, &v, b, g.cell_w));
                            if !it.empty.is_empty() {
                                n.set_placeholder(it.empty.trim_matches(['(', ')']).to_string());
                            }
                            n.set_value(v);
                            n
                        }
                    };
                    n.set_label(it.label.to_string());
                    n.set_position_in_set(i + 1);
                    n.set_size_of_set(s.items.len());
                    n.add_action(Action::Focus);
                    if let Some(b) = row_rect {
                        n.set_bounds(b);
                    }
                    nodes.push((id, n));
                }
                d.set_children(kids);
                nodes.push((SETTINGS, d));
                focus = NodeId(SETTING_BASE + s.sel as u64);
            }
            Overlay::Search(s) => {
                root_children.push(SEARCH);
                let mut n = Node::new(Role::SearchInput);
                n.set_label("Find in scrollback");
                n.set_value(s.query.clone());
                n.set_description(s.status());
                let b = g.cells((0, g.bar_rows + rows - 1, g.cols, 1));
                n.set_bounds(b);
                nodes.push(text_field(&mut n, SEARCH_TEXT, &s.query, b, g.cell_w));
                nodes.push((SEARCH, n));
                focus = SEARCH;
            }
        }

        root_children.push(STATUS);
        let mut status = Node::new(Role::Status);
        status.set_live(Live::Polite);
        status.set_label(ui.announcement.to_string());
        nodes.push((STATUS, status));

        let mut root = Node::new(Role::Window);
        // The dropdown gets a distinct name so Orca's "window activated"
        // announcement says which kind of terminal just appeared.
        root.set_label(if ui.dropdown { format!("Dropdown terminal: {}", ui.title) } else { ui.title.to_string() });
        root.set_children(root_children);
        nodes.push((ROOT, root));

        let tree = (!self.sent_tree).then(|| {
            let mut t = TreeInfo::new(ROOT);
            t.toolkit_name = Some("rsterm".into());
            t.toolkit_version = Some(env!("CARGO_PKG_VERSION").into());
            t
        });
        self.sent_tree = true;
        self.prev_focus = Some(focus);
        TreeUpdate { nodes, tree, tree_id: TreeId::ROOT, focus }
    }
}

impl A11y {
    /// The user's mouse selection (when fully on screen) or else the caret.
    fn term_selection(&self, term: &Term, tid: u64, caret: Option<(usize, usize)>) -> Option<TextSelection> {
        if let Some(sel) = term.selection {
            let ((l0, c0), (l1, c1)) = sel.ordered();
            let top = term.abs_line(0);
            let rows = term.rows() as i64;
            if l0 >= top && l1 < top + rows {
                let pos = |line: i64, col: usize| {
                    let y = (line - top) as usize;
                    let row = term.screen().view_row(y, term.display_offset);
                    let idx = row.cells[..col.min(row.cells.len())]
                        .iter()
                        .filter(|c| c.flags & flags::WIDE_SPACER == 0)
                        .count();
                    let text = &self.prev_lines[y];
                    let len = text.chars().count() - text.ends_with('\n') as usize;
                    TextPosition { node: line_node(tid, y), character_index: idx.min(len) }
                };
                return Some(TextSelection { anchor: pos(l0, c0), focus: pos(l1, c1 + 1) });
            }
        }
        let (y, idx) = caret?;
        let p = TextPosition { node: line_node(tid, y), character_index: idx };
        Some(TextSelection { anchor: p, focus: p })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The Terminal's document text as AccessKit would see it after `ups`.
    fn apply(tree: &mut HashMap<NodeId, Node>, ups: &[TreeUpdate], tid: u64) -> String {
        for u in ups {
            for (id, n) in &u.nodes {
                tree.insert(*id, n.clone());
            }
        }
        tree[&term_node(tid)].children().iter().map(|id| tree[id].value().unwrap_or("").to_string()).collect()
    }

    /// AccessKit's change events: (removed, inserted) by common prefix/suffix.
    fn diff(old: &str, new: &str) -> (String, String) {
        let o: Vec<char> = old.chars().collect();
        let n: Vec<char> = new.chars().collect();
        let p = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
        let s = o[p..].iter().rev().zip(n[p..].iter().rev()).take_while(|(a, b)| a == b).count();
        (o[p..o.len() - s].iter().collect(), n[p..n.len() - s].iter().collect())
    }

    #[test]
    fn scrolling_only_inserts_new_lines() {
        let mut t = Term::new(20, 4);
        let mut p = vte::Parser::new();
        let g = Geometry { cell_w: 8.0, cell_h: 16.0, pad: 0.0, bar_rows: 0, cols: 20, rows: 4 };
        let cfg = Config::default();
        let ui = UiView {
            title: "t",
            tab_id: 0,
            tabs: &[],
            overlay: &Overlay::None,
            cfg: &cfg,
            announcement: "",
            dropdown: true,
        };
        let mut a = A11y::default();
        let mut tree = HashMap::new();
        p.advance(&mut t, b"one\r\ntwo\r\nthree\r\nfour");
        let first = a.update(&t, &g, &ui);
        let mut doc = apply(&mut tree, &[first], 0);
        for (input, spoken) in [("\r\nfive", "\nfive"), ("\r\nsix\r\nseven", "\nsix\nseven")] {
            p.advance(&mut t, input.as_bytes());
            let mut ups = Vec::new();
            if let Some(s) = a.scroll(&t, &g, &ui) {
                let mid = apply(&mut tree, &[s.clone()], 0);
                // Step 1 only removes text at the start.
                assert!(doc.ends_with(&mid), "{doc:?} -> {mid:?}");
                ups.push(s);
            }
            let mid = apply(&mut tree, &ups, 0);
            let up = a.update(&t, &g, &ui);
            let new = apply(&mut tree, &[up], 0);
            assert_eq!(diff(&mid, &new).1, spoken);
            doc = new;
        }
        assert_eq!(doc, "four\nfive\nsix\nseven");
    }
}
