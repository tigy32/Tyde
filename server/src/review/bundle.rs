use protocol::{
    ProjectGitDiffLine, ProjectGitDiffLineKind, ProjectId, Review, ReviewAnchor, ReviewCommentId,
    ReviewCommentSource, ReviewDiffSide, ReviewId, ReviewLocation, ReviewTarget, SessionId,
};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReviewFeedbackBundle {
    pub review_id: ReviewId,
    pub project_id: ProjectId,
    pub origin_session_id: SessionId,
    pub comments: Vec<ReviewFeedbackComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReviewFeedbackComment {
    pub comment_id: ReviewCommentId,
    pub location: ReviewLocation,
    pub body: String,
    pub source: ReviewCommentSource,
    pub excerpt: Vec<ProjectGitDiffLine>,
}

impl ReviewFeedbackBundle {
    pub(crate) fn from_review(review: &Review) -> Result<Self, String> {
        let comments = review
            .comments
            .iter()
            .map(|comment| {
                Ok(ReviewFeedbackComment {
                    comment_id: comment.id.clone(),
                    location: comment.location.clone(),
                    body: comment.body.clone(),
                    source: comment.source.clone(),
                    excerpt: excerpt_for_location(review, &comment.location)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        Ok(Self {
            review_id: review.id.clone(),
            project_id: review.project_id.clone(),
            origin_session_id: review.origin_session_id.clone(),
            comments,
        })
    }

    pub(crate) fn render_markdown(&self) -> Result<String, String> {
        let mut out = String::new();
        out.push_str(&format!(
            "The user finished a review with {} comments. Address each and update the code.\n\n",
            self.comments.len()
        ));
        out.push_str("```tyde-review\n");
        out.push_str(
            &serde_json::to_string_pretty(self)
                .map_err(|err| format!("failed to serialize review feedback bundle: {err}"))?,
        );
        out.push_str("\n```\n");

        for comment in &self.comments {
            out.push('\n');
            out.push_str("### ");
            out.push_str(&location_heading(&comment.location));
            out.push_str(" (");
            out.push_str(source_heading(&comment.source));
            out.push_str(")\n");
            for line in comment.body.lines() {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            if comment.body.is_empty() {
                out.push_str("> \n");
            }

            if !comment.excerpt.is_empty() {
                out.push_str("\n> Original lines:\n> ```\n");
                for line in &comment.excerpt {
                    out.push_str("> ");
                    out.push_str(diff_line_prefix(line.kind));
                    out.push_str(&line.text);
                    out.push('\n');
                }
                out.push_str("> ```\n");
            }
        }

        Ok(out)
    }
}

fn excerpt_for_location(
    review: &Review,
    location: &ReviewLocation,
) -> Result<Vec<ProjectGitDiffLine>, String> {
    if let ReviewTarget::RegularFile { revision } = &location.target {
        let snapshot = review
            .file_snapshots
            .iter()
            .find(|snapshot| {
                snapshot.root == location.root
                    && snapshot.relative_path == location.relative_path
                    && snapshot.revision == *revision
            })
            .ok_or_else(|| {
                format!(
                    "review {} has no file snapshot {} in root {}",
                    review.id, location.relative_path, location.root
                )
            })?;
        return match &location.anchor {
            ReviewAnchor::File => Ok(Vec::new()),
            ReviewAnchor::LineRange {
                start_line,
                end_line,
                ..
            } => Ok((*start_line..=*end_line)
                .filter_map(|line_number| {
                    snapshot
                        .lines
                        .get(line_number.saturating_sub(1) as usize)
                        .map(|text| ProjectGitDiffLine {
                            kind: ProjectGitDiffLineKind::Context,
                            text: text.clone(),
                            old_line_number: None,
                            new_line_number: Some(line_number),
                        })
                })
                .collect()),
            ReviewAnchor::Hunk { .. } => Err("regular files do not have diff hunks".to_owned()),
        };
    }
    let Some(file) = review
        .diffs
        .iter()
        .find(|diff| {
            diff.root == location.root
                && diff.scope
                    == match location.target {
                        ReviewTarget::UnstagedDiff => protocol::ProjectDiffScope::Unstaged,
                        ReviewTarget::StagedDiff => protocol::ProjectDiffScope::Staged,
                        ReviewTarget::RegularFile { .. } => unreachable!(),
                    }
        })
        .and_then(|diff| {
            diff.files
                .iter()
                .find(|file| file.relative_path == location.relative_path)
        })
    else {
        return Err(format!(
            "review {} has no diff file {} in root {}",
            review.id, location.relative_path, location.root
        ));
    };

    match &location.anchor {
        ReviewAnchor::File => Ok(Vec::new()),
        ReviewAnchor::Hunk { hunk_id, .. } => file
            .hunks
            .iter()
            .find(|hunk| &hunk.hunk_id == hunk_id)
            .map(|hunk| hunk.lines.clone())
            .ok_or_else(|| {
                format!(
                    "review {} has no hunk {} in {}",
                    review.id, hunk_id, location.relative_path
                )
            }),
        ReviewAnchor::LineRange {
            side,
            start_line,
            end_line,
        } => Ok(file
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| line_matches_range(line, *side, *start_line, *end_line))
            .cloned()
            .collect()),
    }
}

fn line_matches_range(
    line: &ProjectGitDiffLine,
    side: ReviewDiffSide,
    start_line: u32,
    end_line: u32,
) -> bool {
    match side {
        ReviewDiffSide::Old => line
            .old_line_number
            .is_some_and(|line_number| line_number >= start_line && line_number <= end_line),
        ReviewDiffSide::New => line
            .new_line_number
            .is_some_and(|line_number| line_number >= start_line && line_number <= end_line),
    }
}

fn location_heading(location: &ReviewLocation) -> String {
    let target = location.target.label();
    match &location.anchor {
        ReviewAnchor::File => format!("{} [{target}]", location.relative_path),
        ReviewAnchor::Hunk { hunk_id, .. } => {
            format!("{} hunk {} [{target}]", location.relative_path, hunk_id)
        }
        ReviewAnchor::LineRange {
            side,
            start_line,
            end_line,
        } => {
            let side = match side {
                ReviewDiffSide::Old => "old",
                ReviewDiffSide::New => "new",
            };
            if start_line == end_line {
                format!(
                    "{}:{} ({side}, {target})",
                    location.relative_path, start_line
                )
            } else {
                format!(
                    "{}:{}-{} ({side}, {target})",
                    location.relative_path, start_line, end_line
                )
            }
        }
    }
}

fn source_heading(source: &ReviewCommentSource) -> &'static str {
    match source {
        ReviewCommentSource::User => "user",
        ReviewCommentSource::AiSuggestion { .. } => "ai",
    }
}

fn diff_line_prefix(kind: ProjectGitDiffLineKind) -> &'static str {
    match kind {
        ProjectGitDiffLineKind::Context => " ",
        ProjectGitDiffLineKind::Added => "+",
        ProjectGitDiffLineKind::Removed => "-",
    }
}
