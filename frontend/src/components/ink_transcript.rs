//! Ink keeps one byline per input stretch without hiding or reordering actions.

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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InkRowLayout {
    pub gap: InkGap,
    pub byline: bool,
    pub continuation: bool,
    pub actions_only: bool,
}

pub(crate) fn ink_row_layouts(rows: &[ChatRowHandle]) -> HashMap<ChatRowId, InkRowLayout> {
    let mut layouts = HashMap::with_capacity(rows.len());
    let mut stretch_has_byline = false;
    let mut previous_actions = false;
    let mut previous_assistant = false;
    for (index, row) in rows.iter().enumerate() {
        let mut layout = InkRowLayout::default();
        let (input, assistant, actions_only) = match &row.content {
            ChatRowContent::Message(entry) => entry.with(|entry| {
                let message = &entry.message;
                let input = message.origin.is_some()
                    || matches!(
                        message.sender,
                        MessageSender::User | MessageSender::Agent { .. }
                    );
                let assistant = !input && matches!(message.sender, MessageSender::Assistant { .. });
                let actions_only = assistant
                    && message.content.trim().is_empty()
                    && message.reasoning.is_none()
                    && message
                        .images
                        .as_ref()
                        .is_none_or(|images| images.is_empty())
                    && !entry.tool_requests.is_empty();
                (input, assistant, actions_only)
            }),
            ChatRowContent::Streaming(stream) => (
                false,
                true,
                stream.text.with(|text| text.trim().is_empty())
                    && stream.reasoning.with(|text| text.trim().is_empty())
                    && stream.tool_requests.with(|tools| !tools.is_empty()),
            ),
            ChatRowContent::ContextCompaction(_) | ChatRowContent::Notice(_) => {
                (false, false, false)
            }
        };
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
        if matches!(&row.content, ChatRowContent::Notice(notice) if notice.with(|notice| matches!(notice, crate::state::ChatNotice::RetryAttempt { .. })))
        {
            layout.gap = InkGap::Tight;
        }
        if index == 0 {
            layout.gap = InkGap::None;
        }
        previous_actions = actions_only;
        previous_assistant = assistant;
        layouts.insert(row.id, layout);
    }
    layouts
}
