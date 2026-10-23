//! Ink's grouped transcript: one byline per working stretch and completed
//! actions folded behind a single disclosure.
//!
//! A stretch runs from one input message to the next. Idle boundaries would be
//! the better edge, but `TypingStatusChanged` is not part of replayed history,
//! so a grouping built on it would differ after a reload. Input messages are
//! present both live and in history.
//!
//! The layout is a projection of the row list and each row's own signals; the
//! only state it adds is the user's disclosure choice, kept on the tool
//! entries' presentation like every other tool disclosure.

use std::collections::HashMap;
use std::sync::Arc;

use leptos::prelude::*;
use protocol::{AgentId, MessageSender};

use crate::components::chat_message::{MessageSegment, interleave_message, trailing_tools_index};
use crate::components::tool_card::ink_quiet_action;
use crate::state::{
    AppState, ChatMessageEntry, ChatRowContent, ChatRowHandle, ChatRowId, ToolOutputMode,
    ToolRequestEntry,
};

pub type ActionsDisclosure = ArcRwSignal<(Option<ToolOutputMode>, Option<bool>)>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InkGap {
    #[default]
    Normal,
    Tight,
    None,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkRowLayout {
    pub gap: InkGap,
    /// First assistant row of its stretch: the only one that shows the byline.
    pub byline: bool,
    /// Assistant row after the stretch's byline.
    pub continuation: bool,
    /// Tool calls of this row that fold into a completed-actions disclosure.
    pub quiet: Arc<[String]>,
    pub run: Option<InkActionsRun>,
}

/// Consecutive completed actions shown as one line: rows with nothing but
/// completed actions, optionally led by a row that ends with them.
#[derive(Clone, Debug, PartialEq)]
pub struct InkActionsRun {
    pub head: bool,
    /// The head row has its own prose; its trailing tool group carries the
    /// summary line instead of the top of the row.
    pub lead: bool,
    pub count: usize,
    pub disclosure: ActionsDisclosure,
    pub members: Arc<[ActionsDisclosure]>,
}

impl InkActionsRun {
    pub(crate) fn hidden(&self, mode: ToolOutputMode) -> bool {
        !self.head && !actions_open(&self.disclosure, mode)
    }
}

enum RowKind {
    Input,
    Other,
    Assistant {
        quiet: Vec<String>,
        activity_only: Option<Vec<ActionsDisclosure>>,
        /// Completed actions that end a row with other content.
        trailing: Option<Vec<ActionsDisclosure>>,
    },
}

#[derive(Default)]
struct RunBuilder {
    rows: Vec<ChatRowId>,
    disclosures: Vec<ActionsDisclosure>,
    lead: bool,
}

impl RunBuilder {
    fn flush(&mut self, layouts: &mut HashMap<ChatRowId, InkRowLayout>) {
        let rows = std::mem::take(&mut self.rows);
        let disclosures = std::mem::take(&mut self.disclosures);
        let lead = std::mem::take(&mut self.lead);
        // A lead with no followers keeps its own group summary.
        if lead && rows.len() < 2 {
            return;
        }
        let Some(disclosure) = disclosures.first().cloned() else {
            return;
        };
        let count = disclosures.len();
        let members: Arc<[ActionsDisclosure]> = disclosures.into();
        for (index, id) in rows.into_iter().enumerate() {
            let head = index == 0;
            if let Some(layout) = layouts.get_mut(&id) {
                layout.run = Some(InkActionsRun {
                    head,
                    lead: head && lead,
                    count: if head { count } else { 0 },
                    disclosure: disclosure.clone(),
                    members: if head { members.clone() } else { Arc::new([]) },
                });
            }
        }
    }
}

pub(crate) fn ink_row_layouts(
    state: &AppState,
    agent_id: &AgentId,
    rows: &[ChatRowHandle],
) -> HashMap<ChatRowId, InkRowLayout> {
    let mut layouts = HashMap::with_capacity(rows.len());
    let mut stretch_has_byline = false;
    let mut previous_assistant = false;
    let mut run = RunBuilder::default();
    for (index, row) in rows.iter().enumerate() {
        let mut layout = InkRowLayout::default();
        let kind = match &row.content {
            ChatRowContent::Message(entry) => {
                entry.with(|entry| classify_message(state, agent_id, entry))
            }
            ChatRowContent::Streaming(stream) => RowKind::Assistant {
                quiet: stream.tool_requests.with(|tools| {
                    tools
                        .iter()
                        .filter(|tool| {
                            tool.entry
                                .with(|entry| ink_quiet_action(state, agent_id, entry))
                        })
                        .map(|tool| tool.tool_call_id.clone())
                        .collect()
                }),
                activity_only: None,
                trailing: None,
            },
            ChatRowContent::ContextCompaction(_) | ChatRowContent::Notice(_) => RowKind::Other,
        };
        match kind {
            RowKind::Input => {
                run.flush(&mut layouts);
                stretch_has_byline = false;
                previous_assistant = false;
            }
            RowKind::Other => {
                run.flush(&mut layouts);
                previous_assistant = false;
            }
            RowKind::Assistant {
                quiet,
                activity_only,
                trailing,
            } => {
                layout.byline = !stretch_has_byline;
                layout.continuation = stretch_has_byline;
                if previous_assistant && layout.continuation {
                    layout.gap = InkGap::Tight;
                }
                stretch_has_byline = true;
                previous_assistant = true;
                layout.quiet = quiet.into();
                match (activity_only, trailing) {
                    (Some(disclosures), _) => {
                        run.rows.push(row.id);
                        run.disclosures.extend(disclosures);
                    }
                    (None, Some(disclosures)) => {
                        run.flush(&mut layouts);
                        run.rows.push(row.id);
                        run.disclosures.extend(disclosures);
                        run.lead = true;
                    }
                    (None, None) => run.flush(&mut layouts),
                }
            }
        }
        if index == 0 {
            layout.gap = InkGap::None;
        }
        layouts.insert(row.id, layout);
    }
    run.flush(&mut layouts);
    layouts
}

fn classify_message(state: &AppState, agent_id: &AgentId, entry: &ChatMessageEntry) -> RowKind {
    let message = &entry.message;
    if message.origin.is_some()
        || matches!(
            message.sender,
            MessageSender::User | MessageSender::Agent { .. }
        )
    {
        return RowKind::Input;
    }
    if !matches!(message.sender, MessageSender::Assistant { .. }) {
        return RowKind::Other;
    }
    let quiet: Vec<_> = entry
        .tool_requests
        .iter()
        .filter(|tool| ink_quiet_action(state, agent_id, tool))
        .collect();
    // Reasoning keeps its own disclosure, so a reply carrying it stays
    // visible rather than folding into a run.
    let activity_only = message.content.trim().is_empty()
        && message.reasoning.is_none()
        && message
            .images
            .as_ref()
            .is_none_or(|images| images.is_empty())
        && !entry.tool_requests.is_empty()
        && quiet.len() == entry.tool_requests.len();
    let trailing = if activity_only {
        None
    } else {
        trailing_tool_group(entry).filter(|group| {
            group
                .iter()
                .all(|tool| ink_quiet_action(state, agent_id, tool))
        })
    }
    .map(|group| {
        group
            .iter()
            .map(|tool| tool.presentation.actions_disclosure.clone())
            .collect()
    });
    RowKind::Assistant {
        trailing,
        activity_only: activity_only.then(|| {
            quiet
                .iter()
                .map(|tool| tool.presentation.actions_disclosure.clone())
                .collect()
        }),
        quiet: quiet
            .into_iter()
            .map(|tool| tool.request.tool_call_id.clone())
            .collect(),
    }
}

/// The tool group a message ends with, when nothing visible follows it.
fn trailing_tool_group(entry: &ChatMessageEntry) -> Option<Vec<ToolRequestEntry>> {
    let message = &entry.message;
    let segments = interleave_message(
        &message.content,
        &message.tool_calls,
        entry.tool_requests.clone(),
        message
            .images
            .as_ref()
            .is_some_and(|images| !images.is_empty()),
    );
    let index = trailing_tools_index(&segments)?;
    match segments.into_iter().nth(index) {
        Some(MessageSegment::Tools { entries, .. }) if !entries.is_empty() => Some(entries),
        _ => None,
    }
}

/// Full opens completed actions; the user's choice outranks that default in
/// the output mode where it was made, exactly like a single tool card.
/// Every member of a run carries the same choice, since the summary writes
/// them together; a run without members has nothing to open.
pub(crate) fn run_open(members: &[ActionsDisclosure], mode: ToolOutputMode) -> bool {
    members
        .first()
        .is_some_and(|disclosure| actions_open(disclosure, mode))
}

/// Ink's fold of one tool list: the completed actions behind its summary
/// line, whether that line is open, and whether the list is part of a run.
/// Cards stay mounted either way, so a disclosure the user opened survives
/// folding and switching presentation.
#[derive(Clone, Debug, PartialEq)]
pub struct InkFold {
    pub quiet: Arc<[String]>,
    pub open: bool,
    pub run: bool,
}

impl InkFold {
    pub(crate) fn card(&self, tool_call_id: &str) -> Option<&'static str> {
        self.quiet
            .iter()
            .any(|id| id == tool_call_id)
            .then_some(if self.open { "open" } else { "folded" })
    }
}

/// Ink's presentation of one tool group: its fold and the summary line it
/// shows, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct InkToolGroup {
    pub fold: InkFold,
    pub summary: Option<(usize, Arc<[ActionsDisclosure]>)>,
}

/// A run's lead summarizes the whole run at its trailing group, and its other
/// members list their actions under that line; any other group summarizes
/// only its own completed actions.
pub(crate) fn ink_tool_group(
    layout: &InkRowLayout,
    mode: ToolOutputMode,
    tools: &[(String, ActionsDisclosure)],
    trailing: bool,
) -> InkToolGroup {
    let quiet: Vec<_> = tools
        .iter()
        .filter(|(id, _)| layout.quiet.contains(id))
        .collect();
    let ids: Arc<[String]> = quiet.iter().map(|(id, _)| id.clone()).collect();
    match &layout.run {
        Some(run) if !run.lead || trailing => InkToolGroup {
            fold: InkFold {
                quiet: ids,
                open: actions_open(&run.disclosure, mode),
                run: true,
            },
            summary: run.lead.then(|| (run.count, run.members.clone())),
        },
        _ => {
            let members: Arc<[ActionsDisclosure]> = quiet
                .iter()
                .map(|(_, disclosure)| disclosure.clone())
                .collect();
            InkToolGroup {
                fold: InkFold {
                    quiet: ids,
                    open: run_open(&members, mode),
                    run: false,
                },
                summary: (!members.is_empty()).then(|| (members.len(), members)),
            }
        }
    }
}

/// The summary line a tool group shows before its cards.
#[component]
pub(crate) fn InkGroupSummary(
    group: Memo<Option<InkToolGroup>>,
    mode: RwSignal<ToolOutputMode>,
) -> impl IntoView {
    let summary = Memo::new(move |_| {
        group.with(|group| group.as_ref().and_then(|group| group.summary.clone()))
    });
    view! {
        <Show when=move || summary.with(Option::is_some)>
            <InkActionsSummary
                count=Signal::derive(move || summary.with(|summary| summary.as_ref().map_or(0, |(count, _)| *count)))
                members=Signal::derive(move || summary.with(|summary| summary.as_ref().map(|(_, members)| members.clone()).unwrap_or_else(|| Arc::new([]))))
                mode=mode
            />
        </Show>
    }
}

pub(crate) fn actions_open(disclosure: &ActionsDisclosure, mode: ToolOutputMode) -> bool {
    let (chosen_in, choice) = disclosure.get();
    let default = mode == ToolOutputMode::Full;
    if chosen_in == Some(mode) {
        choice.unwrap_or(default)
    } else {
        default
    }
}

#[component]
pub(crate) fn InkActionsSummary(
    count: Signal<usize>,
    members: Signal<Arc<[ActionsDisclosure]>>,
    mode: RwSignal<ToolOutputMode>,
) -> impl IntoView {
    let open = Memo::new(move |_| members.with(|members| run_open(members, mode.get())));
    view! {
        <button
            type="button"
            class="ink-actions-summary"
            aria-expanded=move || open.get().to_string()
            on:click=move |_| {
                let next = (Some(mode.get_untracked()), Some(!open.get_untracked()));
                members.with_untracked(|members| {
                    for member in members.iter() {
                        member.set(next);
                    }
                });
            }
        >
            <svg class="ink-actions-check" width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
                <path d="M3 8.5l3.2 3L13 4.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" />
            </svg>
            <span class="ink-actions-label">
                {move || {
                    let count = count.get();
                    format!("{count} completed {}", if count == 1 { "action" } else { "actions" })
                }}
            </span>
            <span class="ink-actions-chevron" aria-hidden="true"></span>
        </button>
    }
}
