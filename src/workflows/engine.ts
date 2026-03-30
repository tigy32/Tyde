import {
  collectAgentResult,
  runShellCommand,
  spawnAgent,
  waitForAgent,
} from "../bridge";
import type { WorkflowStore } from "./store";
import type {
  ActionResult,
  ActionRunState,
  StepResult,
  StepRunState,
  WorkflowDefinition,
  WorkflowRunState,
} from "./types";

let runCounter = 0;

function applyTemplateVariables(text: string, previousOutput: string): string {
  return text.replace(/\{\{previous_output\}\}/g, previousOutput);
}

export class WorkflowEngine {
  private workspacePath: string;
  private workspaceRoots: string[];
  private store: WorkflowStore;
  private runs = new Map<string, WorkflowRunState>();
  onChange: ((run: WorkflowRunState) => void) | null = null;

  constructor(
    workspacePath: string,
    workspaceRoots: string[],
    store: WorkflowStore,
  ) {
    this.workspacePath = workspacePath;
    this.workspaceRoots = workspaceRoots;
    this.store = store;
  }

  setWorkspaceRoots(roots: string[]): void {
    this.workspaceRoots = roots;
  }

  async execute(workflow: WorkflowDefinition): Promise<WorkflowRunState> {
    runCounter++;
    const runId = `run-${runCounter}-${Date.now()}`;

    const run: WorkflowRunState = {
      workflow,
      runId,
      status: "running",
      steps: workflow.steps.map((step) => ({
        step,
        status: "pending",
        result: null,
        actions: step.actions.map((action) => ({
          action,
          status: "pending" as const,
          result: null,
        })),
      })),
      result: null,
      startedAt: Date.now(),
      completedAt: null,
    };

    this.runs.set(runId, run);
    this.onChange?.(run);

    let previousOutput = "";

    for (const stepState of run.steps) {
      stepState.status = "running";
      this.onChange?.(run);

      const stepResult = await this.executeStep(
        workflow.name,
        stepState,
        previousOutput,
        run,
      );

      stepState.result = stepResult;
      stepState.status = "completed";
      this.onChange?.(run);

      if (!stepResult.success) {
        run.status = "completed";
        run.result = {
          success: false,
          error: `Step "${stepState.step.name}" failed: ${stepResult.actionResults.find((r) => !r.success)?.error ?? "unknown error"}`,
        };
        run.completedAt = Date.now();
        this.onChange?.(run);
        return run;
      }

      previousOutput = stepResult.output;
    }

    run.status = "completed";
    run.result = { success: true };
    run.completedAt = Date.now();
    this.onChange?.(run);
    return run;
  }

  getActiveRuns(): WorkflowRunState[] {
    return Array.from(this.runs.values()).filter((r) => r.status === "running");
  }

  getAllRuns(): WorkflowRunState[] {
    return Array.from(this.runs.values());
  }

  getRun(runId: string): WorkflowRunState | undefined {
    return this.runs.get(runId);
  }

  private async executeStep(
    workflowName: string,
    stepState: StepRunState,
    previousOutput: string,
    run: WorkflowRunState,
  ): Promise<StepResult> {
    // Run all actions concurrently
    const promises = stepState.actions.map((actionState) =>
      this.executeAction(
        actionState,
        previousOutput,
        workflowName,
        stepState.step.name,
        run,
      ),
    );

    const results = await Promise.all(promises);

    const allSuccess = results.every((r) => r.success);
    const combinedOutput = results.map((r) => r.output).join("\n");

    return {
      output: combinedOutput,
      success: allSuccess,
      actionResults: results,
    };
  }

  private async executeAction(
    actionState: ActionRunState,
    previousOutput: string,
    workflowName: string,
    stepName: string,
    run: WorkflowRunState,
  ): Promise<ActionResult> {
    actionState.status = "running";
    this.onChange?.(run);

    let result: ActionResult;
    switch (actionState.action.type) {
      case "run_command":
        result = await this.executeRunCommand(
          actionState.action.command,
          previousOutput,
        );
        break;
      case "spawn_agent":
        result = await this.executeSpawnAgent(
          actionState,
          actionState.action.prompt,
          `${workflowName} - ${stepName}`,
          previousOutput,
          run,
        );
        break;
      case "run_workflow":
        result = await this.executeRunWorkflow(
          actionState.action.workflowId,
          previousOutput,
        );
        break;
    }

    actionState.result = result;
    actionState.status = "completed";
    this.onChange?.(run);
    return result;
  }

  private async executeRunCommand(
    command: string,
    previousOutput: string,
  ): Promise<ActionResult> {
    const resolvedCommand = applyTemplateVariables(command, previousOutput);
    const result = await runShellCommand(resolvedCommand, this.workspacePath);
    const output = result.stdout + (result.stderr ? `\n${result.stderr}` : "");
    return {
      output,
      success: result.success,
      error: result.success
        ? undefined
        : `Command exited with code ${result.exit_code}: ${result.stderr}`,
    };
  }

  private async executeSpawnAgent(
    actionState: ActionRunState,
    prompt: string,
    name: string,
    previousOutput: string,
    run: WorkflowRunState,
  ): Promise<ActionResult> {
    const resolvedPrompt = applyTemplateVariables(prompt, previousOutput);
    const spawnResult = await spawnAgent(
      this.workspaceRoots,
      resolvedPrompt,
      undefined,
      undefined,
      name,
      true,
    );

    // Store conversationId immediately so the card is clickable while running
    actionState.conversationId = spawnResult.conversation_id;
    this.onChange?.(run);

    await waitForAgent(spawnResult.agent_id);
    const agentResult = await collectAgentResult(spawnResult.agent_id);
    const output = agentResult.final_message ?? "";
    const success = !agentResult.agent.last_error;
    return {
      output,
      success,
      error: agentResult.agent.last_error ?? undefined,
      agentId: spawnResult.agent_id,
      conversationId: spawnResult.conversation_id,
    };
  }

  private async executeRunWorkflow(
    workflowId: string,
    _previousOutput: string,
  ): Promise<ActionResult> {
    const nested = this.store.getById(workflowId);
    if (!nested) {
      return {
        output: "",
        success: false,
        error: `Workflow "${workflowId}" not found`,
      };
    }
    const nestedRun = await this.execute(nested);
    const lastStep = nestedRun.steps[nestedRun.steps.length - 1];
    return {
      output: lastStep?.result?.output ?? "",
      success: nestedRun.result?.success ?? false,
      error: nestedRun.result?.error,
    };
  }
}
