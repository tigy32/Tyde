//! Ink keeps one byline per input stretch without hiding or reordering actions.
//! Errors and warnings annotate the turn they follow, so they sit close to it.

use std::collections::HashMap;

use leptos::prelude::*;
use protocol::MessageSender;

use crate::state::{ChatRowContent, ChatRowHandle, ChatRowId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InkGap {
    #[default]
    Normal,
    Tight,
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InkRowLayout {
    pub gap: InkGap,
    pub byline: bool,
    pub continuation: bool,
    pub actions_only: bool,
    pub is_flow: bool,
    pub show_reasoning: bool,
    pub empty: bool,
}

impl Default for InkRowLayout {
    fn default() -> Self {
        Self {
            gap: InkGap::Normal,
            byline: false,
            continuation: false,
            actions_only: false,
            is_flow: false,
            show_reasoning: true,
            empty: false,
        }
    }
}

pub(crate) fn ink_row_layouts(
    rows: &[ChatRowHandle],
    appearance: crate::appearance::ChatAppearance,
    flow_show_reasoning: bool,
) -> HashMap<ChatRowId, InkRowLayout> {
    let is_flow = appearance == crate::appearance::ChatAppearance::Ink;
    let show_reasoning = !is_flow || flow_show_reasoning;
    let mut layouts = HashMap::with_capacity(rows.len());
    let mut stretch_has_byline = false;
    let mut previous_actions = false;
    let mut previous_assistant = false;
    let mut has_visible_row = false;
    for row in rows {
        let mut layout = InkRowLayout {
            is_flow,
            show_reasoning,
            ..InkRowLayout::default()
        };
        let (input, assistant, actions_only, annotation, empty) = match &row.content {
            ChatRowContent::Message(entry) => entry.with(|entry| {
                let message = &entry.message;
                let input = message.origin.is_some()
                    || matches!(
                        message.sender,
                        MessageSender::User | MessageSender::Agent { .. }
                    );
                let assistant = !input && matches!(message.sender, MessageSender::Assistant { .. });
                let has_text = !message.content.trim().is_empty();
                let has_reasoning = show_reasoning
                    && message.reasoning.as_ref().is_some_and(|r| {
                        !is_flow || !r.text.trim().is_empty() || r.tokens.is_some()
                    });
                let has_images = message
                    .images
                    .as_ref()
                    .is_some_and(|images| !images.is_empty());
                let has_tools = !entry.tool_requests.is_empty();
                let actions_only =
                    assistant && !has_text && !has_reasoning && !has_images && has_tools;
                let empty = is_flow
                    && assistant
                    && !has_text
                    && !has_reasoning
                    && !has_images
                    && !has_tools;
                let annotation = !input
                    && matches!(
                        message.sender,
                        MessageSender::Error | MessageSender::Warning
                    );
                (input, assistant, actions_only, annotation, empty)
            }),
            ChatRowContent::Streaming(stream) => {
                let has_text = stream.text.with(|text| !text.trim().is_empty());
                let has_reasoning =
                    show_reasoning && stream.reasoning.with(|text| !text.trim().is_empty());
                let has_tools = stream.tool_requests.with(|tools| !tools.is_empty());
                let actions_only = !has_text && !has_reasoning && has_tools;
                let empty = is_flow && !has_text && !has_reasoning && !has_tools;
                (false, true, actions_only, false, empty)
            }
            ChatRowContent::ContextCompaction(_) | ChatRowContent::Notice(_) => {
                (false, false, false, false, false)
            }
        };
        if empty {
            layout.empty = true;
            layout.gap = InkGap::None;
            layouts.insert(row.id, layout);
            continue;
        }
        if input {
            stretch_has_byline = false;
        }
        layout.actions_only = actions_only;
        if assistant {
            layout.byline = !stretch_has_byline;
            layout.continuation = stretch_has_byline;
            if layout.continuation {
                layout.gap = if previous_assistant && previous_actions && actions_only {
                    InkGap::None
                } else {
                    InkGap::Tight
                };
            }
            stretch_has_byline = true;
        }
        if annotation
            || matches!(&row.content, ChatRowContent::Notice(notice) if notice.with(|notice| matches!(notice, crate::state::ChatNotice::RetryAttempt { .. })))
        {
            layout.gap = InkGap::Tight;
        }
        if !has_visible_row {
            layout.gap = InkGap::None;
        }
        has_visible_row = true;
        previous_actions = actions_only;
        previous_assistant = assistant;
        layouts.insert(row.id, layout);
    }
    layouts
}
