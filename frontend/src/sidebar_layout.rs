use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[cfg(target_arch = "wasm32")]
pub const STORAGE_KEY: &str = "tyde.desktop-layout.v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterfaceMode {
    #[default]
    Vibe,
    Dinosaur,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarSide {
    Left,
    Right,
    Bottom,
}

impl SidebarSide {
    pub fn column(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 5,
            Self::Bottom => 3,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Bottom => "Bottom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SidebarTab {
    Agents,
    Sessions,
    Files,
    Git,
    Search,
    References,
    Swarms,
    Workflows,
    Terminal,
}

impl SidebarTab {
    pub const ALL: [Self; 9] = [
        Self::Agents,
        Self::Sessions,
        Self::Files,
        Self::Git,
        Self::Search,
        Self::References,
        Self::Swarms,
        Self::Workflows,
        Self::Terminal,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Agents => "Agents",
            Self::Sessions => "History",
            Self::Files => "Files",
            Self::Git => "Git",
            Self::Search => "Search",
            Self::References => "Refs",
            Self::Swarms => "Swarms",
            Self::Workflows => "Workflows",
            Self::Terminal => "Terminal",
        }
    }
    pub fn default_side(self) -> SidebarSide {
        match self {
            Self::Terminal => SidebarSide::Bottom,
            Self::Files | Self::Git | Self::Search | Self::References => SidebarSide::Left,
            Self::Agents | Self::Sessions | Self::Swarms | Self::Workflows => SidebarSide::Right,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarPane {
    pub tabs: Vec<SidebarTab>,
    // Tab visibility changes retain the displayed panel until explicit navigation.
    pub active: Option<SidebarTab>,
    pub visible: bool,
    pub width: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidebarLayout {
    pub left: SidebarPane,
    pub right: SidebarPane,
    #[serde(default = "empty_bottom")]
    pub bottom: SidebarPane,
}

fn empty_bottom() -> SidebarPane {
    SidebarPane {
        tabs: vec![],
        active: None,
        visible: false,
        width: 200.0,
    }
}

impl SidebarLayout {
    pub fn new(mode: InterfaceMode) -> Self {
        let left_tabs = if mode == InterfaceMode::Dinosaur {
            vec![
                SidebarTab::Files,
                SidebarTab::Git,
                SidebarTab::Search,
                SidebarTab::References,
            ]
        } else {
            vec![SidebarTab::Agents, SidebarTab::Sessions]
        };
        let right_tabs = if mode == InterfaceMode::Dinosaur {
            SidebarTab::ALL
                .into_iter()
                .filter(|tab| tab.default_side() == SidebarSide::Right)
                .collect()
        } else {
            vec![]
        };
        Self {
            left: SidebarPane {
                visible: !left_tabs.is_empty(),
                active: left_tabs.first().copied(),
                tabs: left_tabs,
                width: if mode == InterfaceMode::Vibe {
                    360.0
                } else {
                    300.0
                },
            },
            right: SidebarPane {
                active: right_tabs.first().copied(),
                visible: !right_tabs.is_empty(),
                tabs: right_tabs,
                width: 360.0,
            },
            bottom: empty_bottom(),
        }
    }
    pub fn pane(&self, side: SidebarSide) -> &SidebarPane {
        match side {
            SidebarSide::Left => &self.left,
            SidebarSide::Right => &self.right,
            SidebarSide::Bottom => &self.bottom,
        }
    }
    pub fn pane_mut(&mut self, side: SidebarSide) -> &mut SidebarPane {
        match side {
            SidebarSide::Left => &mut self.left,
            SidebarSide::Right => &mut self.right,
            SidebarSide::Bottom => &mut self.bottom,
        }
    }
    pub fn side(&self, tab: SidebarTab) -> Option<SidebarSide> {
        [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom]
            .into_iter()
            .find(|side| self.pane(*side).tabs.contains(&tab))
    }
    pub fn move_tab(&mut self, tab: SidebarTab, to: SidebarSide, before: Option<SidebarTab>) {
        let Some(from) = self.side(tab) else {
            return;
        };
        if before == Some(tab) {
            return;
        }
        let mut index = match before {
            Some(before) => {
                let Some(index) = self.pane(to).tabs.iter().position(|item| *item == before) else {
                    return;
                };
                index
            }
            None => self.pane(to).tabs.len(),
        };
        if from == to
            && self
                .pane(from)
                .tabs
                .iter()
                .take(index)
                .any(|item| *item == tab)
        {
            index -= 1;
        }
        let source = self.pane_mut(from);
        source.tabs.retain(|item| *item != tab);
        if source.active == Some(tab) {
            source.active = source.tabs.first().copied();
        }
        for side in [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom] {
            if self.pane(side).active == Some(tab) {
                let pane = self.pane_mut(side);
                pane.active = pane.tabs.first().copied();
            }
        }
        let target = self.pane_mut(to);
        target.tabs.insert(index, tab);
        target.active = Some(tab);
        target.visible = true;
    }
    #[cfg(target_arch = "wasm32")]
    fn validate(&self) -> Result<(), String> {
        let active = [self.left.active, self.right.active, self.bottom.active];
        for (index, tab) in active.iter().enumerate() {
            if tab.is_some() && active[..index].contains(tab) {
                return Err("Duplicated active sidebar panel".into());
            }
        }
        let mut found = Vec::new();
        for side in [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom] {
            let pane = self.pane(side);
            let minimum = if side == SidebarSide::Bottom {
                80.0
            } else {
                160.0
            };
            if !pane.width.is_finite() || !(minimum..=2400.0).contains(&pane.width) {
                return Err("Invalid sidebar width".into());
            }
            for tab in &pane.tabs {
                if found.contains(tab) {
                    return Err("Duplicated sidebar tab".into());
                }
                found.push(*tab);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientLayouts {
    version: u8,
    pub mode: InterfaceMode,
    pub vibe: SidebarLayout,
    pub dinosaur: SidebarLayout,
}
impl Default for ClientLayouts {
    fn default() -> Self {
        Self {
            version: 2,
            mode: InterfaceMode::Vibe,
            vibe: SidebarLayout::new(InterfaceMode::Vibe),
            dinosaur: SidebarLayout::new(InterfaceMode::Dinosaur),
        }
    }
}
impl ClientLayouts {
    pub fn current(&self) -> &SidebarLayout {
        match self.mode {
            InterfaceMode::Vibe => &self.vibe,
            InterfaceMode::Dinosaur => &self.dinosaur,
        }
    }
    pub fn current_mut(&mut self) -> &mut SidebarLayout {
        match self.mode {
            InterfaceMode::Vibe => &mut self.vibe,
            InterfaceMode::Dinosaur => &mut self.dinosaur,
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn migrate_legacy_sidebar(storage: &web_sys::Storage) -> Result<Option<ClientLayouts>, String> {
    use crate::components::agents_panel::COLLAPSED_FOLDERS_STORAGE_KEY;
    let Some(raw) = storage
        .get_item(COLLAPSED_FOLDERS_STORAGE_KEY)
        .map_err(|error| format!("Cannot read legacy sidebar layout: {error:?}"))?
    else {
        return Ok(None);
    };
    // This is the only sidebar layout record written by the previous UI.
    // Its pane arrangement was fixed; the folder choices remain on their
    // existing canonical key rather than being copied into another store.
    let _: std::collections::HashSet<String> = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "Cannot migrate legacy sidebar folders: {error}. Use Reset sidebar layout in Settings."
        )
    })?;
    let mut layouts = ClientLayouts {
        mode: InterfaceMode::Dinosaur,
        ..ClientLayouts::default()
    };
    layouts.dinosaur.left.width = 320.0;
    layouts.dinosaur.right.width = 320.0;
    Ok(Some(layouts))
}

impl AppState {
    pub fn activate_sidebar_tab(&self, tab: SidebarTab) {
        if self.sidebar_storage_blocked.get_untracked() {
            return;
        }
        self.sidebar_layout.update(|layouts| {
            let side = match layouts.current().side(tab) {
                Some(side) => side,
                None => {
                    let side = tab.default_side();
                    layouts.current_mut().pane_mut(side).tabs.push(tab);
                    side
                }
            };
            for other in [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom] {
                if other != side && layouts.current().pane(other).active == Some(tab) {
                    layouts.current_mut().pane_mut(other).active = None;
                }
            }
            {
                let pane = layouts.current_mut().pane_mut(side);
                pane.active = Some(tab);
                pane.visible = true;
            }
        });
    }
    pub fn toggle_sidebar_tab(&self, tab: SidebarTab, side: SidebarSide) {
        if self.sidebar_storage_blocked.get_untracked() {
            return;
        }
        self.sidebar_layout.update(|layouts| {
            let layout = layouts.current_mut();
            match layout.side(tab) {
                Some(current) => {
                    let pane = layout.pane_mut(current);
                    pane.tabs.retain(|item| *item != tab);
                    if pane.tabs.is_empty() {
                        pane.active = None;
                    }
                }
                None => {
                    for other in [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom] {
                        if other != side && layout.pane(other).active == Some(tab) {
                            let pane = layout.pane_mut(other);
                            pane.active = pane.tabs.first().copied();
                        }
                    }
                    let pane = layout.pane_mut(side);
                    pane.tabs.push(tab);
                    if pane.active.is_none() {
                        pane.active = Some(tab);
                    }
                    pane.visible = true;
                }
            }
        });
    }
    pub fn toggle_sidebar(&self, side: SidebarSide) {
        if self.sidebar_storage_blocked.get_untracked() {
            return;
        }
        self.sidebar_layout.update(|layouts| {
            let pane = layouts.current_mut().pane_mut(side);
            pane.visible = !pane.visible;
        });
    }
    pub fn move_sidebar_tab(&self, tab: SidebarTab, side: SidebarSide, before: Option<SidebarTab>) {
        if self.sidebar_storage_blocked.get_untracked() {
            return;
        }
        self.sidebar_layout
            .update(|layouts| layouts.current_mut().move_tab(tab, side, before));
    }
    #[cfg(target_arch = "wasm32")]
    pub fn install_sidebar_persistence(&self) {
        let restore = || -> Result<Option<ClientLayouts>, String> {
            let storage = web_sys::window()
                .ok_or("No window for layout storage")?
                .local_storage()
                .map_err(|e| format!("Cannot access layout storage: {e:?}"))?
                .ok_or("Layout storage is unavailable")?;
            let Some(raw) = storage
                .get_item(STORAGE_KEY)
                .map_err(|e| format!("Cannot read layout: {e:?}"))?
            else {
                return migrate_legacy_sidebar(&storage);
            };
            let mut layouts: ClientLayouts = serde_json::from_str(&raw).map_err(|e| {
                format!("Cannot read desktop layout: {e}. Use Reset sidebar layout in Settings.")
            })?;
            match layouts.version {
                1 => {
                    // Version 1 could not render an empty pane without an active
                    // view, even when its saved visibility bit was true.
                    for layout in [&mut layouts.vibe, &mut layouts.dinosaur] {
                        for side in [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom] {
                            let pane = layout.pane_mut(side);
                            pane.visible &= pane.active.is_some() || !pane.tabs.is_empty();
                        }
                    }
                    layouts.version = 2;
                }
                2 => {}
                _ => {
                    return Err(
                        "Unsupported desktop layout version. Use Reset sidebar layout in Settings."
                            .into(),
                    );
                }
            }
            // Older layouts retained an active view after its final tab was
            // unchecked. Empty docks now render the drop target instead.
            for layout in [&mut layouts.vibe, &mut layouts.dinosaur] {
                for side in [SidebarSide::Left, SidebarSide::Right, SidebarSide::Bottom] {
                    let pane = layout.pane_mut(side);
                    if pane.tabs.is_empty() {
                        pane.active = None;
                    }
                }
            }
            layouts.vibe.validate()?;
            layouts.dinosaur.validate()?;
            Ok(Some(layouts))
        };
        match restore() {
            Ok(Some(layouts)) => self.sidebar_layout.set(layouts),
            Ok(None) => {}
            Err(error) => {
                self.sidebar_storage_blocked.set(true);
                crate::components::header::report_user_error(error);
            }
        }
        let layout = self.sidebar_layout;
        let blocked = self.sidebar_storage_blocked;
        Effect::new(move |_| {
            if blocked.get() {
                return;
            }
            let result = || -> Result<(), String> {
                let raw = serde_json::to_string(&layout.get()).map_err(|e| e.to_string())?;
                let storage = web_sys::window()
                    .ok_or("No window for layout storage")?
                    .local_storage()
                    .map_err(|e| format!("Cannot access layout storage: {e:?}"))?
                    .ok_or("Layout storage is unavailable")?;
                storage
                    .set_item(STORAGE_KEY, &raw)
                    .map_err(|e| format!("Cannot save desktop layout: {e:?}"))
            }();
            if let Err(error) = result {
                crate::components::header::report_user_error(error);
            }
        });
    }
}
