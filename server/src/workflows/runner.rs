use std::collections::HashMap;

use protocol::{WorkflowRunId, WorkflowSummary};
use serde_json::Value;

pub(crate) fn build_coordinator_prompt(
    run_id: &WorkflowRunId,
    summary: &WorkflowSummary,
    body: &str,
    inputs: &HashMap<String, Value>,
) -> String {
    let inputs_json = serde_json::to_string_pretty(inputs).unwrap_or_else(|_| "{}".to_owned());
    let declared = if summary.declared_backends.is_empty() {
        "No child-agent backends are declared for this workflow.".to_owned()
    } else {
        format!(
            "Declared child-agent backends: {:?}.",
            summary.declared_backends
        )
    };
    format!(
        "You are coordinating a Tyde Workflow.\n\nWorkflow id: {}\nWorkflow run id: {}\nWorkflow name: {}\n{}\n\nInputs (server supplied JSON):\n{}\n\nWorkflow body:\n{}\n\nUse tyde_workflow_report_step to report meaningful progress. Use tyde_workflow_finish exactly once when the workflow is complete or irrecoverably failed. If you spawn child agents, use only the declared child-agent backends; the host enforces this. Do not invent workflow ids or run ids; the host derives them from your agent identity.",
        summary.id.0,
        run_id.0,
        summary.name,
        declared,
        inputs_json,
        body.trim()
    )
}
