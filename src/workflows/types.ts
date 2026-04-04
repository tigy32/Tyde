export type RunCommandAction = {
  type: "run_command";
  command: string;
};

export type SpawnAgentAction = {
  type: "spawn_agent";
  prompt: string;
  name: string;
};

export type RunWorkflowAction = {
  type: "run_workflow";
  workflowId: string;
};

export type WorkflowAction =
  | RunCommandAction
  | SpawnAgentAction
  | RunWorkflowAction;

export interface WorkflowStep {
  name: string;
  actions: WorkflowAction[];
}

export interface WorkflowDefinition {
  id: string;
  name: string;
  description: string;
  trigger: string;
  steps: WorkflowStep[];
}

export type ActionResult = {
  output: string;
  success: boolean;
  error?: string;
  agentId?: string;
  conversationId?: number;
};

export type StepResult = {
  output: string;
  success: boolean;
  actionResults: ActionResult[];
};

export type ActionRunState = {
  action: WorkflowAction;
  status: "pending" | "running" | "completed";
  result: ActionResult | null;
  /** Available at spawn time for agents, before the action completes */
  conversationId?: number;
};

export interface StepRunState {
  step: WorkflowStep;
  status: "pending" | "running" | "completed";
  result: StepResult | null;
  actions: ActionRunState[];
}

export interface WorkflowRunState {
  workflow: WorkflowDefinition;
  runId: string;
  status: "pending" | "running" | "completed";
  steps: StepRunState[];
  result: { success: boolean; error?: string } | null;
  startedAt: number;
  completedAt: number | null;
}
