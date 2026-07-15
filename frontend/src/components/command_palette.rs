use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::actions::begin_new_chat;
use crate::components::center_zone::{MIN_SPLIT_WIDTH, announce, reveal_tab, workspace_width};
use crate::send;
use crate::state::{
    AppState, CENTER_TABS_DISABLED_REASON, DUPLICATE_FILE_NOT_A_FILE_REASON, DockVisibility,
    DuplicateFileEligibility, DuplicateFileResult, MoveTabEligibility, MoveTabResult, OpenTarget,
    PaneId, RightTab, TAB_SOURCE_MISSING_REASON, TabContent, TabId, root_display_name,
};

use protocol::{
    ProjectFileKind, ProjectId, ProjectPath, WorkflowId, WorkflowInputSpec, WorkflowSourceScope,
};

/// Every command the app can execute. Typed rather than string-keyed so
/// execution is an exhaustive match with no unknown-command fallback: adding a
/// command that nothing can run, or running one that does not exist, is a
/// compile error (dev-docs/32 §12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandId {
    NewChat,
    ToggleLeftPanel,
    ToggleRightPanel,
    ToggleBottomPanel,
    OpenWorkflows,
    GoToHome,
    GoToChat,
    ToggleSettings,
    SendFeedback,
    SplitRight,
    MoveTabToOtherPane,
    FocusPrimaryPane,
    FocusSecondaryPane,
    CloseEditorPane,
    CloseOtherPane,
}

/// An action that only exists relative to a focused element — a resource row,
/// a tab, a pane. It has a chord, but that chord is bound *on the element*,
/// never on the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextActionId {
    /// `Command/Ctrl+Enter` on a focused palette result, explorer entry, or
    /// search result.
    OpenToSide,
}

/// Everything the binding table can name. Keeping commands and contextual
/// actions in one typed space is what lets the global-handler generator prove
/// it installs *only* `Global` chords.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionId {
    Command(CommandId),
    Context(ContextActionId),
}

/// The focus context a contextual chord belongs to. A chord in one scope cannot
/// fire in another, which is the only reason `Command/Ctrl+Enter` can mean "open
/// to the side" on a result row and "send / steer" in the composer.
///
/// Only scopes this table actually binds appear here. The composer's chords
/// (send, steer, fork+send) are owned by `chat_input.rs` on its own textarea and
/// are deliberately *not* in this table — that separation is what keeps them
/// from colliding, and a variant nobody constructs would only pretend otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingScope {
    /// A focused resource row: palette results, explorer entries, search hits.
    ResultList,
}

/// A key chord, described once and used for three things: matching a real
/// keyboard event, rendering a platform-correct hint, and proving at test time
/// that no two `Global` bindings collide and that no `Global` binding claims a
/// composer chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    /// True when the chord requires the platform command modifier (⌘ on macOS,
    /// Ctrl elsewhere). One field, because these are the same chord.
    pub cmd_or_ctrl: bool,
    pub shift: bool,
    /// `KeyboardEvent.key`, plus any alternate spelling the same physical key
    /// produces under Shift (US layout turns `\` into `|`).
    pub key: &'static str,
    pub shifted_key: Option<&'static str>,
}

impl Chord {
    const fn cmd(key: &'static str) -> Self {
        Self {
            cmd_or_ctrl: true,
            shift: false,
            key,
            shifted_key: None,
        }
    }

    const fn cmd_shift(key: &'static str, shifted_key: &'static str) -> Self {
        Self {
            cmd_or_ctrl: true,
            shift: true,
            key,
            shifted_key: Some(shifted_key),
        }
    }

    pub fn matches(&self, event: &web_sys::KeyboardEvent) -> bool {
        let cmd_or_ctrl = event.ctrl_key() || event.meta_key();
        if cmd_or_ctrl != self.cmd_or_ctrl || event.shift_key() != self.shift {
            return false;
        }
        let pressed = event.key();
        pressed.eq_ignore_ascii_case(self.key)
            || self
                .shifted_key
                .is_some_and(|alternate| pressed.eq_ignore_ascii_case(alternate))
    }

    /// The hint shown in menus and palette rows, derived from the same chord
    /// that does the matching — so displayed and actual shortcuts cannot drift.
    ///
    /// Reads the running platform, so it only means anything in a browser. The
    /// formatting itself lives in [`Chord::hint_for`], which is pure.
    pub fn hint(&self) -> String {
        self.hint_for(ModifierStyle::current())
    }

    /// Render the hint for an explicit platform style.
    ///
    /// Pure: no browser, no globals, no detection — the platform is an input.
    /// That is what lets the display rules (uppercase keycaps, symbol vs word
    /// modifiers) be tested natively, on both platforms, instead of only in the
    /// one the test happens to run on.
    pub fn hint_for(&self, style: ModifierStyle) -> String {
        let mut hint = String::new();
        if self.cmd_or_ctrl {
            hint.push_str(style.command());
        }
        if self.shift {
            hint.push_str(style.shift());
        }
        // A single letter is shown as the user sees it on the keycap: ⌘N, not
        // ⌘n. Multi-character keys ("Enter") and symbols ("\\") are unchanged.
        let mut chars = self.key.chars();
        match (chars.next(), chars.next()) {
            (Some(single), None) if single.is_alphabetic() => {
                hint.extend(single.to_uppercase());
            }
            _ => hint.push_str(self.key),
        }
        hint
    }
}

/// How a platform prints modifier keys on its keycaps.
///
/// A value, not a lookup: every rendering path takes one, so the browser is
/// consulted exactly once (in [`ModifierStyle::current`]) rather than from
/// inside a formatter that would then be unusable anywhere else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierStyle {
    /// Apple keyboards: ⌘ and ⇧ glyphs, no separator.
    Symbols,
    /// Everywhere else: `Ctrl+`, `Shift+`.
    Words,
}

impl ModifierStyle {
    /// The style of the platform actually running the app.
    ///
    /// **Browser-only.** It reads the user agent, which exists in no other
    /// context; calling it from a native build is a programming error and will
    /// panic on the `js_sys` import rather than quietly guessing a platform.
    /// Native tests must use [`Chord::hint_for`] with an explicit style.
    pub fn current() -> Self {
        let apple = web_sys::window()
            .map(|window| window.navigator())
            .and_then(|navigator| navigator.user_agent().ok())
            .is_some_and(|agent| agent.contains("Mac") || agent.contains("iPhone"));
        if apple { Self::Symbols } else { Self::Words }
    }

    fn command(self) -> &'static str {
        match self {
            Self::Symbols => "⌘",
            Self::Words => "Ctrl+",
        }
    }

    fn shift(self) -> &'static str {
        match self {
            Self::Symbols => "⇧",
            Self::Words => "Shift+",
        }
    }
}

/// Where a chord is listened for.
///
/// This is an enum, not a struct with a `scope` field, so that "a contextual
/// chord cannot be installed globally" is a property of the *type*: the global
/// handler is generated by matching `Binding::Global`, and a `Contextual`
/// binding has no chord it could hand over. Nothing but a compile error can
/// put `Command/Ctrl+Enter` — which the composer owns for send/steer — on the
/// window (plan §4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Binding {
    /// Installed on the window by `app.rs`, generated from this table.
    Global(Chord),
    /// Attached to the focused element that owns the action.
    Contextual { chord: Chord, scope: BindingScope },
}

impl Binding {
    /// The chord, whatever its scope — for rendering a hint, never for
    /// installing a listener.
    pub fn chord(self) -> Chord {
        match self {
            Self::Global(chord) => chord,
            Self::Contextual { chord, .. } => chord,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CommandDescriptor {
    pub id: CommandId,
    pub name: &'static str,
    pub binding: Option<Binding>,
}

impl CommandDescriptor {
    /// The shortcut a row or menu displays. Browser-only, like [`Chord::hint`]:
    /// it renders for the running platform. Anything that needs a hint without a
    /// browser — a native test — goes through [`Chord::hint_for`] with an
    /// explicit [`ModifierStyle`], which is the pure formatter this delegates to.
    pub fn shortcut_hint(&self) -> Option<String> {
        self.binding.map(|binding| binding.chord().hint())
    }
}

/// The single source for command execution, shortcut display, and — through
/// `global_bindings()` — the window keydown handler itself.
pub const COMMANDS: &[CommandDescriptor] = &[
    CommandDescriptor {
        id: CommandId::NewChat,
        name: "New Chat",
        binding: Some(Binding::Global(Chord::cmd("n"))),
    },
    CommandDescriptor {
        id: CommandId::ToggleLeftPanel,
        name: "Toggle Left Panel",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::ToggleRightPanel,
        name: "Toggle Right Panel",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::OpenWorkflows,
        name: "Open Workflows",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::ToggleBottomPanel,
        name: "Toggle Bottom Panel",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::GoToHome,
        name: "Go to Home",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::GoToChat,
        name: "Go to Chat",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::SplitRight,
        name: "Split Right",
        binding: Some(Binding::Global(Chord::cmd("\\"))),
    },
    CommandDescriptor {
        id: CommandId::MoveTabToOtherPane,
        name: "Move Tab to Other Pane",
        binding: Some(Binding::Global(Chord::cmd_shift("\\", "|"))),
    },
    CommandDescriptor {
        id: CommandId::FocusPrimaryPane,
        name: "Focus Primary Pane",
        binding: Some(Binding::Global(Chord::cmd("1"))),
    },
    CommandDescriptor {
        id: CommandId::FocusSecondaryPane,
        name: "Focus Secondary Pane",
        binding: Some(Binding::Global(Chord::cmd("2"))),
    },
    CommandDescriptor {
        id: CommandId::CloseEditorPane,
        name: "Close Editor Pane",
        binding: None,
    },
    CommandDescriptor {
        id: CommandId::CloseOtherPane,
        name: "Return to Single Pane",
        binding: None,
    },
    // `Ctrl+,` has always toggled the settings panel (the pre-split
    // `app.rs` keydown arm did `settings_open.update(|v| *v = !*v)`), while the
    // palette row opened it. One typed command cannot honestly do both, so it
    // keeps the shortcut's long-standing behavior and the palette row is named
    // for what it does.
    CommandDescriptor {
        id: CommandId::ToggleSettings,
        name: "Toggle Settings",
        binding: Some(Binding::Global(Chord::cmd(","))),
    },
    CommandDescriptor {
        id: CommandId::SendFeedback,
        name: "Send Feedback",
        binding: None,
    },
];

/// Contextual actions and their element-scoped chords.
pub const CONTEXT_ACTIONS: &[(ContextActionId, Binding)] = &[(
    ContextActionId::OpenToSide,
    Binding::Contextual {
        chord: Chord::cmd("Enter"),
        scope: BindingScope::ResultList,
    },
)];

pub fn command_descriptor(id: CommandId) -> &'static CommandDescriptor {
    COMMANDS
        .iter()
        .find(|descriptor| descriptor.id == id)
        .expect("every CommandId has a descriptor")
}

pub fn context_binding(id: ContextActionId) -> Binding {
    CONTEXT_ACTIONS
        .iter()
        .find(|(action, _)| *action == id)
        .map(|(_, binding)| *binding)
        .expect("every ContextActionId has a binding")
}

/// The binding for any action, command or contextual. One lookup, so a caller
/// that only wants to *render a hint* never has to know which kind it holds —
/// and cannot accidentally reach for a chord's scope to decide where to install
/// it.
pub fn binding_for(action: ActionId) -> Option<Binding> {
    match action {
        ActionId::Command(id) => command_descriptor(id).binding,
        ActionId::Context(id) => Some(context_binding(id)),
    }
}

/// The chords the window keydown handler is allowed to install. `app.rs`
/// generates its handler from exactly this list, so a contextual chord cannot
/// leak into the global scope by hand-editing a `match` arm.
pub fn global_bindings() -> impl Iterator<Item = (CommandId, Chord)> {
    COMMANDS
        .iter()
        .filter_map(|descriptor| match descriptor.binding {
            Some(Binding::Global(chord)) => Some((descriptor.id, chord)),
            Some(Binding::Contextual { .. }) | None => None,
        })
}

/// Resolve a keyboard event against the generated global table.
pub fn global_command_for(event: &web_sys::KeyboardEvent) -> Option<CommandId> {
    global_bindings().find_map(|(id, chord)| chord.matches(event).then_some(id))
}

/// Whether a command can run right now, and — when it cannot — the specific
/// reason a user is shown instead of a silently missing control
/// (dev-docs/32 §9, §12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandAvailability {
    Enabled,
    Disabled(&'static str),
}

impl CommandAvailability {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Enabled => None,
            Self::Disabled(reason) => Some(reason),
        }
    }
}

/// Width of the center workspace, when it has been measured. `None` means no
/// measurement exists yet (a headless mount, or before the first observer
/// callback); width-dependent commands stay available rather than being
/// disabled by a measurement that never arrived.
fn width_allows_split(workspace_width: Option<f64>) -> bool {
    workspace_width.is_none_or(|width| width >= MIN_SPLIT_WIDTH)
}

/// Availability of a command **aimed at a specific tab** — what a tab context
/// menu needs, since it acts on the tab it was opened for and not on whatever
/// happens to be active. `None` falls back to the focused pane's active tab.
pub fn command_availability_for(
    state: &AppState,
    id: CommandId,
    tab: Option<TabId>,
    workspace_width: Option<f64>,
) -> CommandAvailability {
    match id {
        CommandId::SplitRight => split_right_availability_for(state, tab, workspace_width),
        CommandId::MoveTabToOtherPane => move_tab_availability(state, tab),
        other => command_availability(state, other, workspace_width),
    }
}

pub fn command_availability(
    state: &AppState,
    id: CommandId,
    workspace_width: Option<f64>,
) -> CommandAvailability {
    let is_split = state.center_zone.with(|center_zone| center_zone.is_split());
    match id {
        CommandId::SplitRight => split_right_availability(state, workspace_width),
        CommandId::MoveTabToOtherPane => move_tab_availability(state, None),
        CommandId::FocusPrimaryPane => CommandAvailability::Enabled,
        CommandId::FocusSecondaryPane | CommandId::CloseEditorPane | CommandId::CloseOtherPane => {
            if is_split {
                CommandAvailability::Enabled
            } else {
                CommandAvailability::Disabled("There is only one pane.")
            }
        }
        CommandId::NewChat
        | CommandId::ToggleLeftPanel
        | CommandId::ToggleRightPanel
        | CommandId::ToggleBottomPanel
        | CommandId::OpenWorkflows
        | CommandId::GoToHome
        | CommandId::GoToChat
        | CommandId::ToggleSettings
        | CommandId::SendFeedback => CommandAvailability::Enabled,
    }
}

/// Shared with the state layer rather than re-typed here, so a control's
/// disabled reason and a refused action's reason cannot drift apart.
const TABS_DISABLED_REASON: &str = CENTER_TABS_DISABLED_REASON;
const TOO_NARROW_REASON: &str =
    "Not enough width to split — widen the window or hide a side panel.";
const NO_PROJECT_REASON: &str = "Open a project first.";
const NO_OTHER_PANE_REASON: &str =
    "Split the workspace first — there is no other pane to move this to.";
const NO_TAB_REASON: &str = "Open a tab to move.";

/// The pane a tab would move into: the other side of the pane that holds it.
fn move_target(state: &AppState, tab: TabId) -> Option<PaneId> {
    state
        .center_zone
        .with(|center_zone| center_zone.locate_tab(tab).map(PaneId::other))
}

/// Why a specific tab can or cannot move to the other pane.
///
/// The eligibility question is answered by the state layer's typed
/// `MoveTabEligibility` *before* acting, and its `disabled_reason()` is the text
/// the user sees — so the reason shown on a disabled control and the reason a
/// refused move reports are literally the same value. Only the two conditions
/// the layout cannot express (tabs disabled, no second pane) are decided here.
pub fn move_tab_availability(state: &AppState, tab: Option<TabId>) -> CommandAvailability {
    if !state.tabs_enabled.get() {
        return CommandAvailability::Disabled(TABS_DISABLED_REASON);
    }
    if !state.center_zone.with(|center_zone| center_zone.is_split()) {
        return CommandAvailability::Disabled(NO_OTHER_PANE_REASON);
    }
    let Some(tab) = tab.or_else(|| {
        state
            .center_zone
            .with(|center_zone| center_zone.active_tab_id())
    }) else {
        return CommandAvailability::Disabled(NO_TAB_REASON);
    };
    let Some(target) = move_target(state, tab) else {
        return CommandAvailability::Disabled(NO_TAB_REASON);
    };
    match state.move_tab_eligibility(target, tab).disabled_reason() {
        Some(reason) => CommandAvailability::Disabled(reason),
        None => CommandAvailability::Enabled,
    }
}

/// Move a tab, and let the typed result decide what the user is told: a
/// successful move is announced, and a refusal announces the state layer's own
/// reason instead of failing silently. Every move path — command, menu, drag —
/// funnels through here, so they cannot report different things.
pub fn move_tab(state: &AppState, target: PaneId, tab: TabId) -> MoveTabResult {
    let label = state.center_zone.with_untracked(|center_zone| {
        center_zone
            .tab(tab)
            .map(|tab| tab.label.clone())
            .unwrap_or_default()
    });
    let result = state.move_tab_to(target, tab);
    match result.disabled_reason() {
        Some(reason) => announce(reason),
        None => announce(format!(
            "Moved {label} to the {} pane.",
            match target {
                PaneId::Primary => "primary",
                PaneId::Secondary => "secondary",
            }
        )),
    }
    result
}

/// The occurrence of `tab`'s resource that already sits in the other pane, as
/// reported by the typed eligibility check. A refused move or drag highlights
/// it, so "already open over there" is something the user can see and not only
/// read.
pub fn conflicting_occurrence(state: &AppState, tab: TabId) -> Option<(PaneId, TabId)> {
    let target = move_target(state, tab)?;
    match state.move_tab_eligibility(target, tab) {
        MoveTabEligibility::ResourceAlreadyInTarget { existing } => Some((target, existing)),
        _ => None,
    }
}

/// Can the workspace host a second pane right now? Shared by every
/// side-targeting action so the palette, the explorer, the tab menus, and the
/// shortcuts can never disagree about why one is unavailable.
///
/// The width check applies only when a split does not exist yet: an existing
/// split already has its other pane, and narrow mode (not a disabled command)
/// is what handles a workspace that has since become too small.
fn split_capacity_reason(state: &AppState, workspace_width: Option<f64>) -> Option<&'static str> {
    if !state.tabs_enabled.get() {
        return Some(TABS_DISABLED_REASON);
    }
    split_creation_availability(state, workspace_width).reason()
}

/// Can the workspace *create* a second pane at this width?
///
/// This is the one condition the state layer cannot own: it is a property of the
/// rendered window, not of the layout. Exported so surfaces outside the center
/// zone — the agents panel and agent monitor, which pair it with the state
/// layer's `AgentOpenToSideResult` — ask the same question and get the same
/// sentence, instead of each inventing a width rule.
///
/// **An existing split stays actionable.** Only *creating* a pane needs the
/// width; once two panes exist, a workspace that has since shrunk is handled by
/// reversible narrow mode, never by disabling the action.
pub fn split_creation_availability(
    state: &AppState,
    workspace_width: Option<f64>,
) -> CommandAvailability {
    let is_split = state.center_zone.with(|center_zone| center_zone.is_split());
    if !is_split && !width_allows_split(workspace_width) {
        return CommandAvailability::Disabled(TOO_NARROW_REASON);
    }
    CommandAvailability::Enabled
}

/// Whether a resource list row (palette result, explorer entry) may offer
/// "Open to the Side". Files open beside only within the active project
/// (dev-docs/32 §6: there is no cross-project split affordance).
pub fn open_to_side_availability(
    state: &AppState,
    workspace_width: Option<f64>,
) -> CommandAvailability {
    if state.active_project.get().is_none() {
        return CommandAvailability::Disabled(NO_PROJECT_REASON);
    }
    match split_capacity_reason(state, workspace_width) {
        Some(reason) => CommandAvailability::Disabled(reason),
        None => CommandAvailability::Enabled,
    }
}

/// dev-docs/32 §9 gives each *kind* of un-splittable tab its own sentence,
/// because each one points at the action that will actually work. The state
/// layer answers the same question with a single `NotAFile`, and its
/// "Only files can be split." is the right words for every kind *except* Home
/// (which has no "instead" to offer) and a chat (whose "instead" is Move to
/// Other Pane — the one gesture that builds file | agent).
///
/// So this is presentation only: the *decision* is the state layer's, and this
/// refines the wording of one variant. It never decides splittability.
fn not_a_file_reason_for(state: &AppState, tab: TabId) -> &'static str {
    match state
        .center_zone
        .with(|center_zone| center_zone.tab(tab).map(|tab| tab.content.clone()))
    {
        Some(TabContent::Home) | None => "Open a file to split.",
        Some(TabContent::Chat { .. }) => {
            "Chats can't be split — only files can appear in both panes. Use Move to \
             Other Pane to put this chat beside a file."
        }
        Some(TabContent::Diff { .. })
        | Some(TabContent::Comments { .. })
        | Some(TabContent::Workflow { .. })
        | Some(TabContent::AgentMonitor) => {
            "Only files can be split. Use Move to Other Pane to put this beside something."
        }
        // The state layer said "not a file" about a File tab: the two layers
        // disagree. Fall back to its own wording rather than inventing one.
        Some(TabContent::File { .. }) => DUPLICATE_FILE_NOT_A_FILE_REASON,
    }
}

/// Whether `Split Right` can run.
///
/// **Every state-owned refusal comes from `DuplicateFileEligibility`** — tabs
/// disabled, missing source tab, not a file, not loaded — including its reason
/// strings. The UI adds exactly one condition the state layer cannot know,
/// because it is a property of the rendered window rather than of the layout:
/// the 645px minimum width. (dev-docs/32 §9, §11.)
///
/// `TargetAlreadyContainsResource` is *enabled*, not refused: Split Right is
/// idempotent and focuses the occurrence already there.
pub fn split_right_availability(
    state: &AppState,
    workspace_width: Option<f64>,
) -> CommandAvailability {
    let tab = state
        .center_zone
        .with(|center_zone| center_zone.active_tab_id());
    split_right_availability_for(state, tab, workspace_width)
}

/// Split Right aimed at an explicit tab.
///
/// A tab context menu acts on **the tab it was opened for**, which is often not
/// the focused pane's active tab — right-clicking a background tab, or a tab in
/// the other pane, must ask about *that* tab. Passing `None` means "the focused
/// pane's active tab", which is what the shortcut and the palette want.
pub fn split_right_availability_for(
    state: &AppState,
    tab: Option<TabId>,
    workspace_width: Option<f64>,
) -> CommandAvailability {
    let Some(source) = tab.or_else(|| {
        state
            .center_zone
            .with(|center_zone| center_zone.active_tab_id())
    }) else {
        return CommandAvailability::Disabled("Open a file to split.");
    };

    let Some(target) = split_target(state, source) else {
        return CommandAvailability::Disabled(TAB_SOURCE_MISSING_REASON);
    };
    let eligibility = state.duplicate_file_eligibility_in(target, source);
    if let Some(reason) = eligibility.disabled_reason() {
        return CommandAvailability::Disabled(match eligibility {
            DuplicateFileEligibility::NotAFile => not_a_file_reason_for(state, source),
            _ => reason,
        });
    }
    split_creation_availability(state, workspace_width)
}

/// The pane a tab would be split into: the other side of the pane holding it.
fn split_target(state: &AppState, tab: TabId) -> Option<PaneId> {
    state
        .center_zone
        .with(|center_zone| center_zone.locate_tab(tab).map(PaneId::other))
}

/// Duplicate `tab` into the other pane, matching the typed result exhaustively.
/// Every Split Right path — shortcut, palette, tab menu, strip button — runs
/// through here, so they cannot succeed or refuse differently.
pub fn split_right(state: &AppState, tab: TabId, workspace_width: Option<f64>) {
    if let CommandAvailability::Disabled(reason) =
        split_right_availability_for(state, Some(tab), workspace_width)
    {
        announce(reason);
        return;
    }
    let Some(target) = split_target(state, tab) else {
        announce(TAB_SOURCE_MISSING_REASON);
        return;
    };
    match state.duplicate_file_in_result(target, tab) {
        DuplicateFileResult::Duplicated { .. } | DuplicateFileResult::ActivatedExisting { .. } => {}
        refused => {
            log::error!(
                "Split Right was enabled for tab {tab:?} but the state layer refused it \
                 with {refused:?}; UI availability and DuplicateFileEligibility disagree"
            );
            if let Some(reason) = refused.disabled_reason() {
                announce(reason);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum PaletteResult {
    File {
        name: String,
        path: ProjectPath,
        display_path: String,
        root_label: String,
    },
    Command {
        entry_index: usize,
    },
    WorkflowRun {
        host_id: String,
        workflow_id: WorkflowId,
        project_id: Option<ProjectId>,
        name: String,
        inputs: Vec<WorkflowInputSpec>,
    },
}

fn fuzzy_score(query: &str, target: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query_lower = query.to_lowercase();
    let target_lower = target.to_lowercase();

    if target_lower.starts_with(&query_lower) {
        return Some(100);
    }

    let words: Vec<&str> = target_lower.split(['/', '.', '_', '-', ' ']).collect();
    for word in &words {
        if word.starts_with(&query_lower) {
            return Some(75);
        }
    }

    if target_lower.contains(&query_lower) {
        return Some(50);
    }

    None
}

fn toggle_dock(signal: RwSignal<DockVisibility>) {
    signal.update(|v: &mut DockVisibility| {
        *v = match v {
            DockVisibility::Visible => DockVisibility::Hidden,
            DockVisibility::Hidden => DockVisibility::Visible,
        };
    });
}

/// Run a command. Refuses to act when the command is unavailable, so a stale
/// menu row, a shortcut pressed in the wrong context, and a palette selection
/// all fail the same way — visibly disabled, never a surprise mutation.
pub fn execute_command(state: &AppState, id: CommandId, workspace_width: Option<f64>) {
    if let CommandAvailability::Disabled(reason) = command_availability(state, id, workspace_width)
    {
        // Refusing silently is what makes a disabled control feel broken. Every
        // refusal — shortcut, menu item, palette row — says the same specific
        // thing through the one polite live region.
        log::debug!("command {id:?} is unavailable: {reason}");
        announce(reason);
        return;
    }
    match id {
        CommandId::NewChat => {
            begin_new_chat(state, None);
        }
        CommandId::ToggleLeftPanel => toggle_dock(state.left_dock),
        CommandId::ToggleRightPanel => toggle_dock(state.right_dock),
        CommandId::ToggleBottomPanel => toggle_dock(state.bottom_dock),
        CommandId::OpenWorkflows => {
            state.right_dock.set(DockVisibility::Visible);
            state.right_tab.set(RightTab::Workflows);
        }
        CommandId::GoToHome => state.open_tab(TabContent::Home, "Home".to_string(), false),
        // "Go to Chat" means: take me to the chat I would be typing into.
        // In priority order — the chat that owns the composer (which may be in
        // either pane), then a chat hidden behind another tab in the focused
        // pane, then a chat in the other pane. Only when the workspace holds no
        // chat at all does this create one, and it creates exactly one draft.
        CommandId::GoToChat => {
            // In narrow mode only the focused pane is on screen — the other pane
            // is `display: none`. Its chat may still own the composer, but the
            // user cannot see that composer, so treating it as "the chat you are
            // already typing into" would throw them into a pane they did not
            // know existed. When the workspace can only show one pane, the chat
            // in *that* pane wins, exactly as in an unsplit workspace.
            let narrow = state.center_zone.with_untracked(|center_zone| {
                center_zone.is_split() && !width_allows_split(workspace_width)
            });
            let target = state.center_zone.with_untracked(|center_zone| {
                let focused = center_zone.focused_id();
                let newest_chat_in = |pane| {
                    center_zone.pane(pane).and_then(|pane| {
                        pane.tabs
                            .iter()
                            .rev()
                            .find(|tab| matches!(tab.content, TabContent::Chat { .. }))
                            .map(|tab| tab.id)
                    })
                };
                if !narrow && let Some((_, owner)) = center_zone.composer_owner() {
                    return Some(owner);
                }
                newest_chat_in(focused).or_else(|| newest_chat_in(focused.other()))
            });
            match target {
                Some(tab) => {
                    reveal_tab(state, tab);
                }
                None => begin_new_chat(state, None),
            }
        }
        CommandId::SplitRight => {
            let Some(source) = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
            else {
                return;
            };
            split_right(state, source, workspace_width);
        }
        CommandId::MoveTabToOtherPane => {
            let Some(active) = state
                .center_zone
                .with_untracked(|center_zone| center_zone.active_tab_id())
            else {
                return;
            };
            let Some(target) = move_target(state, active) else {
                return;
            };
            move_tab(state, target, active);
        }
        CommandId::FocusPrimaryPane => {
            state.focus_pane(PaneId::Primary);
        }
        CommandId::FocusSecondaryPane => {
            state.focus_pane(PaneId::Secondary);
        }
        CommandId::CloseEditorPane => {
            let focused = state
                .center_zone
                .with_untracked(|center_zone| center_zone.focused_id());
            state.close_pane(focused);
        }
        CommandId::CloseOtherPane => state.close_other_pane(),
        CommandId::ToggleSettings => {
            state.command_palette_open.set(false);
            state.settings_open.update(|open| *open = !*open);
        }
        CommandId::SendFeedback => {
            state.command_palette_open.set(false);
            state.feedback_open.set(true);
        }
    }
}

/// Perform the select action for a given result index.
/// Uses expect_context to avoid capturing the non-Copy AppState.
///
/// `open_to_side` is the `Command/Ctrl+Enter` chord. It is meaningful only for
/// openable resource rows (files); on a command or workflow row it falls back
/// to the ordinary action, and it is never registered as a global binding
/// (dev-docs/32 §12).
fn do_select(
    results: Memo<Vec<PaletteResult>>,
    idx: usize,
    workspace_width: Option<f64>,
    open_to_side: bool,
    notice: RwSignal<Option<&'static str>>,
) {
    let state = expect_context::<AppState>();
    let items = results.get();
    if idx >= items.len() {
        return;
    }
    match &items[idx] {
        PaletteResult::File { path, .. } => {
            if open_to_side {
                // A contextual chord that cannot run does *not* quietly fall
                // back to the ordinary action: the user asked for the side, and
                // silently opening in the focused pane would put the file
                // somewhere they did not ask for. Refuse, say exactly why, and
                // leave the palette open so the row is still there to retry.
                if let CommandAvailability::Disabled(reason) =
                    open_to_side_availability(&state, workspace_width)
                {
                    notice.set(Some(reason));
                    announce(reason);
                    return;
                }
                crate::actions::open_project_path_at(&state, path.clone(), OpenTarget::Beside);
            } else {
                crate::actions::open_project_path_at(&state, path.clone(), OpenTarget::Focused);
            }
        }
        PaletteResult::Command { entry_index } => {
            let id = COMMANDS[*entry_index].id;
            if let CommandAvailability::Disabled(reason) =
                command_availability(&state, id, workspace_width)
            {
                notice.set(Some(reason));
                announce(reason);
                return;
            }
            execute_command(&state, id, workspace_width);
        }
        PaletteResult::WorkflowRun {
            host_id,
            workflow_id,
            project_id,
            name,
            inputs,
        } => {
            // A workflow that declares inputs must collect them first: route it
            // through the same global inputs modal the panel uses, instead of
            // firing the trigger with an empty input map. Inputless workflows
            // run in one step.
            if inputs.is_empty() {
                let host_stream = state
                    .host_streams
                    .with_untracked(|streams| streams.get(host_id).cloned());
                if let Some(host_stream) = host_stream {
                    let host_id = host_id.clone();
                    let workflow_id = workflow_id.clone();
                    let project_id = project_id.clone();
                    spawn_local(async move {
                        if let Err(error) = send::trigger_workflow(
                            &host_id,
                            host_stream,
                            workflow_id,
                            project_id,
                            std::collections::HashMap::new(),
                        )
                        .await
                        {
                            log::error!("failed to trigger workflow from palette: {error}");
                        }
                    });
                }
            } else {
                state
                    .workflow_run_request
                    .set(Some(crate::state::WorkflowRunRequest {
                        host_id: host_id.clone(),
                        workflow_id: workflow_id.clone(),
                        project_id: project_id.clone(),
                        name: name.clone(),
                        inputs: inputs.clone(),
                    }));
            }
        }
    }
    state.command_palette_open.set(false);
}

#[component]
pub fn CommandPalette() -> impl IntoView {
    let state = expect_context::<AppState>();
    let open = state.command_palette_open;
    let file_tree = state.file_tree;
    let active_project = state.active_project;
    let workflow_state = state.clone();

    let input = RwSignal::new(String::new());
    let selected_index = RwSignal::new(0usize);

    let is_command_mode = Memo::new(move |_| input.get().starts_with('>'));

    let results: Memo<Vec<PaletteResult>> = Memo::new(move |_| {
        let query_raw = input.get();
        let command_mode = query_raw.starts_with('>');

        if command_mode {
            let query = query_raw[1..].trim();
            let mut scored: Vec<(PaletteResult, u32)> = COMMANDS
                .iter()
                .enumerate()
                .filter_map(|(i, cmd)| {
                    if query.is_empty() {
                        Some((PaletteResult::Command { entry_index: i }, 0))
                    } else {
                        fuzzy_score(query, cmd.name)
                            .map(|s| (PaletteResult::Command { entry_index: i }, s))
                    }
                })
                .collect();
            let active_project_ref = workflow_state.active_project.get();
            let active_host_id = active_project_ref
                .as_ref()
                .map(|active| active.host_id.clone())
                .or_else(|| workflow_state.selected_host_id.get());
            if let Some(host_id) = active_host_id {
                let active_project_id = active_project_ref
                    .as_ref()
                    .map(|active| active.project_id.clone());
                let summaries = workflow_state
                    .workflow_summaries
                    .with(|map| map.get(&host_id).cloned().unwrap_or_default());
                // Run only the workflows effective for the active context: a
                // project workflow shadows the same-id global in its project, so
                // the palette never lists or triggers the wrong definition.
                let workflows = crate::components::workflows_panel::effective_summaries(
                    &summaries,
                    active_project_id.as_ref(),
                );
                for workflow in workflows {
                    let label = format!("Run Workflow {}", workflow.name);
                    let Some(score) = (if query.is_empty() {
                        Some(0)
                    } else {
                        fuzzy_score(query, &label).or_else(|| fuzzy_score(query, &workflow.id.0))
                    }) else {
                        continue;
                    };
                    let project_id = match &workflow.source.scope {
                        WorkflowSourceScope::Project { project_id, .. } => Some(project_id.clone()),
                        WorkflowSourceScope::Global => active_project_id.clone(),
                    };
                    scored.push((
                        PaletteResult::WorkflowRun {
                            host_id: host_id.clone(),
                            workflow_id: workflow.id,
                            project_id,
                            name: workflow.name,
                            inputs: workflow.inputs,
                        },
                        score,
                    ));
                }
            }
            scored.sort_by_key(|score| std::cmp::Reverse(score.1));
            scored
                .into_iter()
                .take(10)
                .map(|(result, _)| result)
                .collect()
        } else {
            let query = query_raw.trim();
            let tree = file_tree.get();
            let Some(active_project) = active_project.get() else {
                return Vec::new();
            };
            let mut scored: Vec<(String, ProjectPath, String, String, u32)> = Vec::new();
            if let Some(root_listings) = tree.get(&active_project.project_id) {
                for root_listing in root_listings {
                    let root_label = root_display_name(&root_listing.root);
                    for entry in &root_listing.entries {
                        if entry.kind != ProjectFileKind::File {
                            continue;
                        }
                        let path = &entry.relative_path;
                        let file_name = path.rsplit('/').next().unwrap_or(path);
                        let score = if query.is_empty() {
                            Some(0)
                        } else {
                            fuzzy_score(query, file_name).or_else(|| fuzzy_score(query, path))
                        };
                        if let Some(s) = score {
                            scored.push((
                                file_name.to_owned(),
                                ProjectPath {
                                    root: root_listing.root.clone(),
                                    relative_path: path.clone(),
                                },
                                path.clone(),
                                root_label.clone(),
                                s,
                            ));
                        }
                    }
                }
            }
            scored.sort_by(|a, b| {
                b.4.cmp(&a.4)
                    .then_with(|| a.3.cmp(&b.3))
                    .then_with(|| a.2.cmp(&b.2))
            });
            scored
                .into_iter()
                .take(10)
                .map(
                    |(name, path, display_path, root_label, _)| PaletteResult::File {
                        name,
                        path,
                        display_path,
                        root_label,
                    },
                )
                .collect()
        }
    });

    let result_count = Memo::new(move |_| results.get().len());
    let width = workspace_width();
    // The last refusal, shown inline on the still-open surface.
    let notice: RwSignal<Option<&'static str>> = RwSignal::new(None);
    let open_to_side_chord = binding_for(ActionId::Context(ContextActionId::OpenToSide))
        .expect("Open to the Side has a binding")
        .chord();

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        // The Open-to-the-Side chord is matched from the same typed chord that
        // renders its hint, and it is handled *here* — on the focused resource
        // list — because no global binding may claim Command/Ctrl+Enter (the
        // chat composer needs it for send/steer).
        if open_to_side_chord.matches(&ev) {
            ev.prevent_default();
            ev.stop_propagation();
            do_select(
                results,
                selected_index.get_untracked(),
                width.get_untracked(),
                true,
                notice,
            );
            return;
        }
        match ev.key().as_str() {
            "Escape" => {
                ev.prevent_default();
                open.set(false);
            }
            "ArrowDown" => {
                ev.prevent_default();
                notice.set(None);
                let count = result_count.get();
                if count > 0 {
                    selected_index.update(|i: &mut usize| *i = (*i + 1) % count);
                }
            }
            "ArrowUp" => {
                ev.prevent_default();
                notice.set(None);
                let count = result_count.get();
                if count > 0 {
                    selected_index.update(|i: &mut usize| {
                        *i = if *i == 0 { count - 1 } else { *i - 1 };
                    });
                }
            }
            "Enter" => {
                ev.prevent_default();
                do_select(
                    results,
                    selected_index.get_untracked(),
                    width.get_untracked(),
                    false,
                    notice,
                );
            }
            _ => {}
        }
    };

    let on_input = move |ev: web_sys::Event| {
        let target = ev.target().unwrap();
        let el: web_sys::HtmlInputElement = target.unchecked_into();
        input.set(el.value());
        selected_index.set(0);
        notice.set(None);
    };

    let on_backdrop_click = move |_| {
        open.set(false);
    };

    let input_ref = NodeRef::<leptos::html::Input>::new();

    Effect::new(move |_| {
        if open.get() {
            input.set(String::new());
            selected_index.set(0);
            notice.set(None);
            if let Some(el) = input_ref.get() {
                let _ = el.focus();
            }
        }
    });

    let mode_label = move || {
        if is_command_mode.get() {
            "Commands"
        } else {
            "Files"
        }
    };

    view! {
        <Show when=move || open.get()>
            <div class="cp-overlay" on:click=on_backdrop_click>
                <div class="cp-modal" on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                    <div class="cp-header">
                        <input
                            node_ref=input_ref
                            class="cp-input"
                            type="text"
                            placeholder="Search files... (type > for commands)"
                            on:input=on_input
                            on:keydown=on_keydown
                            prop:value=move || input.get()
                            spellcheck="false"
                            {..leptos::attr::custom::custom_attribute("autocorrect", "off")}
                            autocapitalize="none"
                            autocomplete="off"
                        />
                        <span class="cp-mode-badge">{mode_label}</span>
                    </div>
                    // A refused action explains itself here, on the surface the
                    // user is still looking at, and is announced politely.
                    <Show when=move || notice.get().is_some()>
                        <div class="cp-notice" role="status" data-testid="cp-notice">
                            {move || notice.get().unwrap_or_default()}
                        </div>
                    </Show>
                    <div class="cp-results">
                        {move || {
                            results.get().into_iter().enumerate().map(|(idx, result)| {
                                let is_selected = move || selected_index.get() == idx;
                                let on_click = move |ev: web_sys::MouseEvent| {
                                    selected_index.set(idx);
                                    let to_side = ev.ctrl_key() || ev.meta_key();
                                    do_select(results, idx, width.get_untracked(), to_side, notice);
                                };
                                match result {
                                    PaletteResult::File {
                                        name,
                                        display_path,
                                        root_label,
                                        ..
                                    } => {
                                        // Both activations are advertised on the row itself: Enter
                                        // opens, the contextual chord opens to the side. The hint
                                        // is rendered from the same chord that fires it.
                                        let side_hint = open_to_side_chord.hint();
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                aria-keyshortcuts="Enter Control+Enter Meta+Enter"
                                                on:click=on_click
                                            >
                                                <span class="cp-file-name">{name}</span>
                                                <span class="cp-root-label">{root_label}</span>
                                                <span class="cp-file-path">{display_path}</span>
                                                <Show when=is_selected>
                                                    <span class="cp-row-hints" data-testid="cp-row-hints">
                                                        <kbd class="cp-cmd-shortcut">"Enter"</kbd>
                                                        <span class="cp-hint-label">"Open"</span>
                                                        <kbd class="cp-cmd-shortcut">{side_hint.clone()}</kbd>
                                                        <span class="cp-hint-label">"Open to the Side"</span>
                                                    </span>
                                                </Show>
                                            </div>
                                        }.into_any()
                                    }
                                    PaletteResult::Command { entry_index } => {
                                        let cmd = &COMMANDS[entry_index];
                                        let name = cmd.name;
                                        // Hint and matcher come from the one typed chord, so the
                                        // key a user is told to press is the key that runs.
                                        let shortcut = cmd.shortcut_hint();
                                        let command_id = cmd.id;
                                        let reason_id = format!("cp-reason-{entry_index}");
                                        // A command that can't run right now stays listed, stays
                                        // selectable, and explains itself instead of disappearing.
                                        let availability_state = expect_context::<AppState>();
                                        let availability = Memo::new(move |_| {
                                            command_availability(
                                                &availability_state,
                                                command_id,
                                                width.get(),
                                            )
                                        });
                                        let disabled = move || !availability.get().is_enabled();
                                        let reason = move || {
                                            availability.get().reason().unwrap_or_default().to_owned()
                                        };
                                        let described_by = {
                                            let reason_id = reason_id.clone();
                                            move || disabled().then(|| reason_id.clone())
                                        };
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                class:disabled=disabled
                                                aria-disabled=move || disabled().then_some("true")
                                                aria-describedby=described_by
                                                title=reason
                                                // Selection (and therefore Enter) still reaches a
                                                // disabled row: `do_select` refuses it and states
                                                // the reason, rather than the row being inert.
                                                on:click=on_click
                                            >
                                                <span class="cp-cmd-name">{name}</span>
                                                <Show when=disabled>
                                                    <span class="cp-cmd-reason" id=reason_id.clone()>
                                                        {reason}
                                                    </span>
                                                </Show>
                                                {shortcut.map(|hint| view! {
                                                    <kbd class="cp-cmd-shortcut">{hint}</kbd>
                                                })}
                                            </div>
                                        }.into_any()
                                    }
                                    PaletteResult::WorkflowRun { name, .. } => {
                                        view! {
                                            <div
                                                class="cp-result-item"
                                                class:selected=is_selected
                                                on:click=on_click
                                            >
                                                <span class="cp-cmd-name">{format!("Run Workflow: {name}")}</span>
                                                <span class="cp-file-path">"Workflows"</span>
                                            </div>
                                        }.into_any()
                                    }
                                }
                            }).collect_view()
                        }}
                        <Show when=move || results.get().is_empty()>
                            <div class="cp-empty">"No results"</div>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The chords a keyboard event could confuse. Case-insensitive on the key,
    /// because that is how `Chord::matches` compares.
    fn chord_signature(chord: &Chord) -> (bool, bool, String) {
        (
            chord.cmd_or_ctrl,
            chord.shift,
            chord.key.to_ascii_lowercase(),
        )
    }

    /// The composer owns `Command/Ctrl+Enter` (send, and steer while thinking)
    /// and `Command/Ctrl+Shift+Enter` (fork + send). A global binding that
    /// claimed either would silently break sending — so the table itself is
    /// what forbids it, not a convention someone has to remember.
    #[test]
    fn no_global_binding_claims_a_composer_chord() {
        for (id, chord) in global_bindings() {
            assert!(
                !chord.key.eq_ignore_ascii_case("Enter"),
                "{id:?} binds Enter globally, which collides with the chat \
                 composer's send/steer chord"
            );
        }
    }

    /// Open to the Side is contextual by construction: it exists only relative
    /// to a focused resource row, and is never installed on the window.
    #[test]
    fn open_to_side_is_contextual_and_uses_the_composer_free_chord() {
        let binding = context_binding(ContextActionId::OpenToSide);
        assert_eq!(
            binding,
            Binding::Contextual {
                chord: Chord::cmd("Enter"),
                scope: BindingScope::ResultList,
            },
            "Open to the Side is scoped to a focused result row, not the window"
        );
        assert!(
            !global_bindings()
                .any(|(_, chord)| chord_signature(&chord) == chord_signature(&binding.chord())),
            "the Open-to-the-Side chord must not also exist as a global binding"
        );
    }

    /// Two global commands on the same chord would make one of them
    /// unreachable, and which one wins would depend on table order.
    #[test]
    fn global_bindings_do_not_collide() {
        let mut seen: Vec<((bool, bool, String), CommandId)> = Vec::new();
        for (id, chord) in global_bindings() {
            let signature = chord_signature(&chord);
            if let Some((_, other)) = seen.iter().find(|(existing, _)| *existing == signature) {
                panic!("{id:?} and {other:?} both bind {signature:?} globally");
            }
            seen.push((signature, id));
        }
    }

    /// The move shortcut is part of the contract with the user; it is not an
    /// accident of the table.
    #[test]
    fn move_tab_keeps_its_cmd_shift_backslash_shortcut() {
        let binding = command_descriptor(CommandId::MoveTabToOtherPane)
            .binding
            .expect("Move Tab to Other Pane has a shortcut");
        assert_eq!(
            binding,
            Binding::Global(Chord::cmd_shift("\\", "|")),
            "the move shortcut is Cmd/Ctrl+Shift+\\, and US layouts report the \
             shifted key as '|', so the chord must accept both spellings"
        );
    }

    /// A keycap is labelled `N`, not `n`. The hint must read the way the key is
    /// printed — on **both** platforms, which this now asserts exactly rather
    /// than through `ends_with`.
    ///
    /// It renders through `hint_for`, the pure formatter, so it exercises the
    /// display rules without touching the browser-only platform detection. That
    /// is the whole point of the split: the rule under test is not a property of
    /// the machine running the test.
    #[test]
    fn letter_keys_are_shown_uppercase_on_every_platform() {
        assert_eq!(Chord::cmd("n").hint_for(ModifierStyle::Symbols), "⌘N");
        assert_eq!(Chord::cmd("n").hint_for(ModifierStyle::Words), "Ctrl+N");
        assert_eq!(
            Chord::cmd_shift("\\", "|").hint_for(ModifierStyle::Symbols),
            "⌘⇧\\"
        );
        assert_eq!(
            Chord::cmd_shift("\\", "|").hint_for(ModifierStyle::Words),
            "Ctrl+Shift+\\"
        );

        // Named keys and symbols are printed verbatim — uppercasing applies to
        // single letters only.
        for style in [ModifierStyle::Symbols, ModifierStyle::Words] {
            assert!(Chord::cmd("Enter").hint_for(style).ends_with("Enter"));
            assert!(Chord::cmd(",").hint_for(style).ends_with(','));
            assert!(Chord::cmd("1").hint_for(style).ends_with('1'));
        }

        // Uppercasing is presentation only: the bound key stays lowercase, and
        // the matcher compares case-insensitively (asserted against a real
        // KeyboardEvent in `a_letter_chord_matches_either_case`).
        assert_eq!(
            Chord::cmd("n").key,
            "n",
            "display must not rewrite the key the matcher compares"
        );
    }

    /// Display hints are derived from the bound chord, never typed by hand.
    ///
    /// The load-bearing claim is the *lookup*: the binding a row resolves for a
    /// command must be the same binding whose chord renders that row's hint. (A
    /// descriptor's own hint delegating to its chord is true by construction —
    /// asserting it proves nothing.) Checked on both platform styles through the
    /// pure formatter, so no browser is involved.
    #[test]
    fn shortcut_hints_come_from_the_bound_chord() {
        for style in [ModifierStyle::Symbols, ModifierStyle::Words] {
            for descriptor in COMMANDS {
                let resolved = binding_for(ActionId::Command(descriptor.id));
                assert_eq!(
                    resolved, descriptor.binding,
                    "{:?}: the lookup a row performs must land on this command's \
                     own binding",
                    descriptor.id
                );
                match descriptor.binding {
                    Some(binding) => {
                        let hint = binding.chord().hint_for(style);
                        assert!(
                            !hint.is_empty(),
                            "{:?} is bound, so it must render a hint ({style:?})",
                            descriptor.id
                        );
                        assert_eq!(
                            resolved.expect("a bound command resolves").chord(),
                            binding.chord(),
                            "{:?}: the chord that renders the hint must be the chord \
                             that fires",
                            descriptor.id
                        );
                    }
                    None => assert!(
                        resolved.is_none(),
                        "{:?} has no binding, so it must advertise no shortcut",
                        descriptor.id
                    ),
                }
            }
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::*;
    use crate::state::ActiveProjectRef;
    use leptos::mount::mount_to;
    use protocol::{FileEntryOp, ProjectFileEntry, ProjectRootListing, ProjectRootPath};
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::HtmlElement;

    wasm_bindgen_test_configure!(run_in_browser);

    async fn settle() {
        for _ in 0..3 {
            let promise = js_sys::Promise::new(&mut |resolve, _reject| {
                web_sys::window()
                    .unwrap()
                    .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0)
                    .unwrap();
            });
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        }
    }

    fn make_container() -> HtmlElement {
        let document = web_sys::window().unwrap().document().unwrap();
        let container = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&container).unwrap();
        container.dyn_into::<HtmlElement>().unwrap()
    }

    fn ctrl_enter() -> web_sys::KeyboardEvent {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key("Enter");
        init.set_ctrl_key(true);
        init.set_bubbles(true);
        init.set_cancelable(true);
        web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap()
    }

    fn chord_event(key: &str, ctrl: bool, shift: bool) -> web_sys::KeyboardEvent {
        let init = web_sys::KeyboardEventInit::new();
        init.set_key(key);
        init.set_ctrl_key(ctrl);
        init.set_shift_key(shift);
        web_sys::KeyboardEvent::new_with_keyboard_event_init_dict("keydown", &init).unwrap()
    }

    /// The other half of the display contract, asserted where a real
    /// `KeyboardEvent` exists: uppercasing the *hint* must not change what the
    /// chord *matches*. A browser reports whichever case the platform sends.
    #[wasm_bindgen_test]
    async fn a_letter_chord_matches_either_case() {
        let chord = Chord::cmd("n");
        assert!(
            chord.matches(&chord_event("n", true, false)),
            "the key as typed"
        );
        assert!(
            chord.matches(&chord_event("N", true, false)),
            "and as the platform may report it — display uppercases, matching \
             must not care"
        );
        assert!(
            !chord.matches(&chord_event("n", false, false)),
            "the modifier is still required"
        );
        assert!(
            !chord.matches(&chord_event("m", true, false)),
            "and it is still the right key"
        );

        // The generated global table routes either case to the same command.
        assert_eq!(
            global_command_for(&chord_event("N", true, false)),
            Some(CommandId::NewChat)
        );
    }

    /// `Ctrl+,` has always toggled the settings panel — that is what the
    /// pre-split global keydown arm did. The typed table must route the chord
    /// to a command that still toggles, not to one that only opens.
    #[wasm_bindgen_test]
    async fn ctrl_comma_routes_globally_and_still_toggles_settings() {
        let state = AppState::new();

        assert_eq!(
            global_command_for(&chord_event(",", true, false)),
            Some(CommandId::ToggleSettings),
            "the generated global handler routes Ctrl+, to the settings command"
        );

        execute_command(&state, CommandId::ToggleSettings, None);
        assert!(
            state.settings_open.get_untracked(),
            "the first press opens settings"
        );
        execute_command(&state, CommandId::ToggleSettings, None);
        assert!(
            !state.settings_open.get_untracked(),
            "the second press closes it again — the shortcut toggles, as it \
             always has"
        );
    }

    /// A chord nobody can discover may as well not exist. The selected result
    /// row advertises both of its activations, and the hint is rendered from
    /// the same chord that fires.
    #[wasm_bindgen_test]
    async fn the_selected_result_row_advertises_open_and_open_to_the_side() {
        let container = make_container();
        let state = AppState::new();
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "h1".to_owned(),
            project_id: ProjectId("proj-1".to_owned()),
        }));
        state.file_tree.update(|tree| {
            tree.insert(
                ProjectId("proj-1".to_owned()),
                vec![ProjectRootListing {
                    root: ProjectRootPath("/repo".to_owned()),
                    // The shape the server actually emits: one entry per path,
                    // `Add` for a path present in the listing
                    // (server/src/project_stream.rs). A directory rides along so
                    // the fixture proves the palette honors the protocol's
                    // `kind` rather than treating every entry as a file.
                    entries: vec![
                        ProjectFileEntry {
                            relative_path: "main.rs".to_owned(),
                            kind: ProjectFileKind::File,
                            op: FileEntryOp::Add,
                        },
                        ProjectFileEntry {
                            relative_path: "src".to_owned(),
                            kind: ProjectFileKind::Directory,
                            op: FileEntryOp::Add,
                        },
                    ],
                }],
            );
        });
        state.command_palette_open.set(true);

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <CommandPalette /> }
        });
        settle().await;

        // Only the File-kind entry is openable: a directory is not a result.
        assert_eq!(
            container
                .query_selector_all(".cp-result-item")
                .unwrap()
                .length(),
            1,
            "the listing holds a file and a directory; only the file may be \
             offered as a result"
        );
        let row: HtmlElement = container
            .query_selector(".cp-result-item")
            .unwrap()
            .expect("the file result row")
            .dyn_into()
            .unwrap();
        assert!(
            row.text_content().unwrap_or_default().contains("main.rs"),
            "and it is the file, not the directory"
        );
        assert!(
            row.get_attribute("aria-keyshortcuts")
                .is_some_and(|keys| keys.contains("Enter") && keys.contains("Control+Enter")),
            "the row exposes both activations to assistive technology"
        );

        let hints = container
            .query_selector("[data-testid=\"cp-row-hints\"]")
            .unwrap()
            .expect("the selected row shows its hints")
            .text_content()
            .unwrap_or_default();
        assert!(
            hints.contains("Open") && hints.contains("Open to the Side"),
            "both activations are named, got {hints:?}"
        );
        let side_hint = context_binding(ContextActionId::OpenToSide).chord().hint();
        assert!(
            hints.contains(&side_hint),
            "the side hint is rendered from the chord that fires it ({side_hint}), \
             got {hints:?}"
        );
    }

    /// An Open-to-the-Side chord that cannot run must NOT quietly open the file
    /// in the focused pane instead. The user asked for the side; putting the
    /// file somewhere else and saying nothing is the worst of both outcomes.
    /// It refuses, states the exact reason on the surface, and the palette
    /// stays open so the row can be retried.
    #[wasm_bindgen_test]
    async fn contextual_chord_refusal_keeps_the_palette_open_and_says_why() {
        let container = make_container();
        let state = AppState::new();
        // Tabs disabled ⇒ there can be no second pane, so Open to the Side is
        // unavailable while an ordinary open would still work.
        state.tabs_enabled.set(false);
        state.active_project.set(Some(ActiveProjectRef {
            host_id: "h1".to_owned(),
            project_id: ProjectId("proj-1".to_owned()),
        }));
        state.file_tree.update(|tree| {
            tree.insert(
                ProjectId("proj-1".to_owned()),
                vec![ProjectRootListing {
                    root: ProjectRootPath("/repo".to_owned()),
                    // The shape the server actually emits: one entry per path,
                    // `Add` for a path present in the listing
                    // (server/src/project_stream.rs). A directory rides along so
                    // the fixture proves the palette honors the protocol's
                    // `kind` rather than treating every entry as a file.
                    entries: vec![
                        ProjectFileEntry {
                            relative_path: "main.rs".to_owned(),
                            kind: ProjectFileKind::File,
                            op: FileEntryOp::Add,
                        },
                        ProjectFileEntry {
                            relative_path: "src".to_owned(),
                            kind: ProjectFileKind::Directory,
                            op: FileEntryOp::Add,
                        },
                    ],
                }],
            );
        });
        state.command_palette_open.set(true);
        let tabs_before = state
            .center_zone
            .with_untracked(|center_zone| center_zone.all_tab_ids().len());

        let state_for_mount = state.clone();
        let _handle = mount_to(container.clone(), move || {
            provide_context(state_for_mount.clone());
            view! { <CommandPalette /> }
        });
        settle().await;

        let input: HtmlElement = container
            .query_selector(".cp-input")
            .unwrap()
            .expect("palette input")
            .dyn_into()
            .unwrap();
        input.dispatch_event(&ctrl_enter()).unwrap();
        settle().await;

        assert!(
            state.command_palette_open.get_untracked(),
            "the surface stays open after a refused contextual chord"
        );
        assert!(
            container.query_selector(".cp-modal").unwrap().is_some(),
            "the palette is still rendered"
        );
        let notice = container
            .query_selector("[data-testid=\"cp-notice\"]")
            .unwrap()
            .expect("the refusal is shown on the surface")
            .text_content()
            .unwrap_or_default();
        assert_eq!(
            notice.trim(),
            "Enable tabs to use split view.",
            "the exact reason is shown, not a generic failure"
        );
        assert_eq!(
            state
                .center_zone
                .with_untracked(|center_zone| center_zone.all_tab_ids().len()),
            tabs_before,
            "the file was NOT opened in the focused pane as a consolation: an \
             unavailable contextual chord never falls back to the ordinary action"
        );
    }
}
