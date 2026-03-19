import type { WorkflowStore } from "./store";
import type { WorkflowAction, WorkflowDefinition, WorkflowStep } from "./types";

function generateId(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
}

export class WorkflowBuilder {
  private overlay: HTMLElement;
  private store: WorkflowStore;
  private editing: WorkflowDefinition | null = null;
  private scope: "global" | "project" = "global";
  private steps: WorkflowStep[] = [];
  private nameInput!: HTMLInputElement;
  private descriptionInput!: HTMLInputElement;
  private triggerInput!: HTMLInputElement;
  private stepsContainer!: HTMLElement;

  onClose: (() => void) | null = null;

  constructor(store: WorkflowStore) {
    this.store = store;

    // Remove stale overlay from previous HMR cycle
    document.querySelector(".workflow-builder-overlay")?.remove();

    this.overlay = document.createElement("div");
    this.overlay.className = "workflow-builder-overlay hidden";
    this.overlay.addEventListener("keydown", (e) => {
      if (e.key === "Escape") this.hide();
    });
    document.body.appendChild(this.overlay);
  }

  showManager(): void {
    this.overlay.innerHTML = "";

    const dialog = document.createElement("div");
    dialog.className = "workflow-builder";

    const header = document.createElement("div");
    header.className = "workflow-builder-header";
    header.innerHTML = "<h2>Manage Workflows</h2>";

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "workflow-builder-close";
    closeBtn.textContent = "\u2715";
    closeBtn.addEventListener("click", () => this.hide());
    header.appendChild(closeBtn);
    dialog.appendChild(header);

    const body = document.createElement("div");
    body.className = "workflow-builder-form";

    const newBtn = document.createElement("button");
    newBtn.type = "button";
    newBtn.className = "workflow-builder-save";
    newBtn.style.alignSelf = "flex-start";
    newBtn.textContent = "+ New Workflow";
    newBtn.addEventListener("click", () => this.show());
    body.appendChild(newBtn);

    const workflows = this.store.getAll();

    if (workflows.length === 0) {
      const empty = document.createElement("div");
      empty.className = "workflow-builder-hint";
      empty.textContent = "No workflows yet. Create one to get started.";
      body.appendChild(empty);
    } else {
      const list = document.createElement("div");
      list.className = "workflow-manager-list";

      for (const workflow of workflows) {
        const row = document.createElement("div");
        row.className = "workflow-manager-row";

        const info = document.createElement("div");
        info.className = "workflow-manager-info";

        const name = document.createElement("span");
        name.className = "workflow-manager-name";
        name.textContent = workflow.name;
        info.appendChild(name);

        const trigger = document.createElement("span");
        trigger.className = "workflow-manager-trigger";
        trigger.textContent = workflow.trigger;
        info.appendChild(trigger);

        row.appendChild(info);

        const actions = document.createElement("div");
        actions.className = "workflow-manager-actions";

        const editBtn = document.createElement("button");
        editBtn.type = "button";
        editBtn.className = "workflow-manager-action-btn";
        editBtn.textContent = "\u270E";
        editBtn.title = "Edit";
        editBtn.addEventListener("click", () => this.show(workflow));
        actions.appendChild(editBtn);

        const deleteBtn = document.createElement("button");
        deleteBtn.type = "button";
        deleteBtn.className =
          "workflow-manager-action-btn workflow-manager-action-delete";
        deleteBtn.textContent = "\u2715";
        deleteBtn.title = "Delete";
        deleteBtn.addEventListener("click", () => {
          this.store.delete(workflow.id).then(() => this.showManager());
        });
        actions.appendChild(deleteBtn);

        row.appendChild(actions);
        list.appendChild(row);
      }

      body.appendChild(list);
    }

    dialog.appendChild(body);
    this.overlay.appendChild(dialog);
    this.overlay.classList.remove("hidden");
  }

  show(workflow?: WorkflowDefinition): void {
    this.editing = workflow ?? null;
    if (workflow) {
      this.steps = JSON.parse(JSON.stringify(workflow.steps));
      this.scope = this.store.getScope(workflow.id);
    } else {
      this.steps = [
        { name: "Step 1", actions: [{ type: "run_command", command: "" }] },
      ];
      this.scope = "global";
    }
    this.buildForm();
    this.overlay.classList.remove("hidden");
  }

  hide(): void {
    this.overlay.classList.add("hidden");
    this.onClose?.();
  }

  private buildForm(): void {
    this.overlay.innerHTML = "";

    const dialog = document.createElement("div");
    dialog.className = "workflow-builder";

    const header = document.createElement("div");
    header.className = "workflow-builder-header";
    header.innerHTML = `<h2>${this.editing ? "Edit Workflow" : "New Workflow"}</h2>`;

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.className = "workflow-builder-close";
    closeBtn.textContent = "\u2715";
    closeBtn.addEventListener("click", () => this.hide());
    header.appendChild(closeBtn);

    dialog.appendChild(header);

    const form = document.createElement("div");
    form.className = "workflow-builder-form";

    form.appendChild(
      this.buildField("Name", () => {
        this.nameInput = document.createElement("input");
        this.nameInput.type = "text";
        this.nameInput.className = "workflow-builder-input";
        this.nameInput.placeholder = "e.g. Git Pull & Rebase";
        this.nameInput.value = this.editing?.name ?? "";
        this.nameInput.addEventListener("input", () => {
          if (!this.editing && this.triggerInput) {
            const id = generateId(this.nameInput.value);
            this.triggerInput.value = id ? `/${id}` : "";
          }
        });
        return this.nameInput;
      }),
    );

    form.appendChild(
      this.buildField("Description", () => {
        this.descriptionInput = document.createElement("input");
        this.descriptionInput.type = "text";
        this.descriptionInput.className = "workflow-builder-input";
        this.descriptionInput.placeholder =
          "Brief description of what this workflow does";
        this.descriptionInput.value = this.editing?.description ?? "";
        return this.descriptionInput;
      }),
    );

    form.appendChild(
      this.buildField("Slash Command", () => {
        this.triggerInput = document.createElement("input");
        this.triggerInput.type = "text";
        this.triggerInput.className = "workflow-builder-input";
        this.triggerInput.placeholder = "/my-workflow";
        this.triggerInput.value = this.editing?.trigger ?? "";
        return this.triggerInput;
      }),
    );

    form.appendChild(
      this.buildField("Scope", () => {
        const scopeRow = document.createElement("div");
        scopeRow.className = "workflow-builder-scope";

        const globalLabel = document.createElement("label");
        globalLabel.className = "workflow-builder-scope-option";
        const globalRadio = document.createElement("input");
        globalRadio.type = "radio";
        globalRadio.name = "workflow-scope";
        globalRadio.value = "global";
        globalRadio.checked = this.scope === "global";
        globalRadio.addEventListener("change", () => {
          this.scope = "global";
        });
        globalLabel.appendChild(globalRadio);
        globalLabel.appendChild(document.createTextNode(" Global"));
        scopeRow.appendChild(globalLabel);

        const projectLabel = document.createElement("label");
        projectLabel.className = "workflow-builder-scope-option";
        const projectRadio = document.createElement("input");
        projectRadio.type = "radio";
        projectRadio.name = "workflow-scope";
        projectRadio.value = "project";
        projectRadio.checked = this.scope === "project";
        projectRadio.addEventListener("change", () => {
          this.scope = "project";
        });
        projectLabel.appendChild(projectRadio);
        projectLabel.appendChild(document.createTextNode(" Project"));
        scopeRow.appendChild(projectLabel);

        return scopeRow;
      }),
    );

    const hint = document.createElement("div");
    hint.className = "workflow-builder-hint";
    hint.textContent =
      "Use {{previous_output}} in prompts or commands to reference the previous step's output.";
    form.appendChild(hint);

    this.stepsContainer = document.createElement("div");
    this.stepsContainer.className = "workflow-builder-steps";
    this.renderSteps();
    form.appendChild(this.stepsContainer);

    const addStepBtn = document.createElement("button");
    addStepBtn.type = "button";
    addStepBtn.className = "workflow-builder-add-step";
    addStepBtn.textContent = "+ Add Step";
    addStepBtn.addEventListener("click", () => {
      this.steps.push({
        name: `Step ${this.steps.length + 1}`,
        actions: [{ type: "run_command", command: "" }],
      });
      this.renderSteps();
    });
    form.appendChild(addStepBtn);

    dialog.appendChild(form);

    const footer = document.createElement("div");
    footer.className = "workflow-builder-footer";

    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "workflow-builder-cancel";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", () => this.hide());
    footer.appendChild(cancelBtn);

    const saveBtn = document.createElement("button");
    saveBtn.type = "button";
    saveBtn.className = "workflow-builder-save";
    saveBtn.textContent = "Save Workflow";
    saveBtn.addEventListener("click", () => this.save());
    footer.appendChild(saveBtn);

    dialog.appendChild(footer);
    this.overlay.appendChild(dialog);
  }

  private buildField(
    label: string,
    createInput: () => HTMLElement,
  ): HTMLElement {
    const field = document.createElement("div");
    field.className = "workflow-builder-field";

    const labelEl = document.createElement("label");
    labelEl.className = "workflow-builder-label";
    labelEl.textContent = label;
    field.appendChild(labelEl);

    field.appendChild(createInput());
    return field;
  }

  private renderSteps(): void {
    this.stepsContainer.innerHTML = "";

    for (let i = 0; i < this.steps.length; i++) {
      const step = this.steps[i];
      const stepEl = document.createElement("div");
      stepEl.className = "workflow-builder-step";

      const stepHeader = document.createElement("div");
      stepHeader.className = "workflow-builder-step-header";

      const stepNumber = document.createElement("span");
      stepNumber.className = "workflow-builder-step-number";
      stepNumber.textContent = `${i + 1}`;
      stepHeader.appendChild(stepNumber);

      const stepNameInput = document.createElement("input");
      stepNameInput.type = "text";
      stepNameInput.className = "workflow-builder-step-name-input";
      stepNameInput.value = step.name;
      stepNameInput.placeholder = "Step name";
      stepNameInput.addEventListener("input", () => {
        step.name = stepNameInput.value;
      });
      stepHeader.appendChild(stepNameInput);

      const stepActions = document.createElement("div");
      stepActions.className = "workflow-builder-step-actions-row";

      if (i > 0) {
        const moveUpBtn = document.createElement("button");
        moveUpBtn.type = "button";
        moveUpBtn.className = "workflow-builder-step-move";
        moveUpBtn.textContent = "\u2191";
        moveUpBtn.title = "Move up";
        moveUpBtn.addEventListener("click", () => {
          [this.steps[i - 1], this.steps[i]] = [
            this.steps[i],
            this.steps[i - 1],
          ];
          this.renderSteps();
        });
        stepActions.appendChild(moveUpBtn);
      }

      if (i < this.steps.length - 1) {
        const moveDownBtn = document.createElement("button");
        moveDownBtn.type = "button";
        moveDownBtn.className = "workflow-builder-step-move";
        moveDownBtn.textContent = "\u2193";
        moveDownBtn.title = "Move down";
        moveDownBtn.addEventListener("click", () => {
          [this.steps[i], this.steps[i + 1]] = [
            this.steps[i + 1],
            this.steps[i],
          ];
          this.renderSteps();
        });
        stepActions.appendChild(moveDownBtn);
      }

      if (this.steps.length > 1) {
        const removeStepBtn = document.createElement("button");
        removeStepBtn.type = "button";
        removeStepBtn.className = "workflow-builder-step-remove";
        removeStepBtn.textContent = "\u2715";
        removeStepBtn.title = "Remove step";
        removeStepBtn.addEventListener("click", () => {
          this.steps.splice(i, 1);
          this.renderSteps();
        });
        stepActions.appendChild(removeStepBtn);
      }

      stepHeader.appendChild(stepActions);
      stepEl.appendChild(stepHeader);

      const actionsEl = document.createElement("div");
      actionsEl.className = "workflow-builder-actions";

      for (let j = 0; j < step.actions.length; j++) {
        actionsEl.appendChild(this.buildActionEditor(step, j));
      }

      const addActionBtn = document.createElement("button");
      addActionBtn.type = "button";
      addActionBtn.className = "workflow-builder-add-action";
      addActionBtn.textContent = "+ Add Action";
      addActionBtn.addEventListener("click", () => {
        step.actions.push({ type: "run_command", command: "" });
        this.renderSteps();
      });
      actionsEl.appendChild(addActionBtn);

      stepEl.appendChild(actionsEl);
      this.stepsContainer.appendChild(stepEl);
    }
  }

  private buildActionEditor(
    step: WorkflowStep,
    actionIndex: number,
  ): HTMLElement {
    const action = step.actions[actionIndex];
    const wrapper = document.createElement("div");
    wrapper.className = "workflow-builder-action";

    const actionHeader = document.createElement("div");
    actionHeader.className = "workflow-builder-action-header";

    const typeSelect = document.createElement("select");
    typeSelect.className = "workflow-builder-action-type";

    const types: Array<{ value: WorkflowAction["type"]; label: string }> = [
      { value: "run_command", label: "Run Command" },
      { value: "spawn_agent", label: "Spawn Agent" },
      { value: "run_workflow", label: "Run Workflow" },
    ];

    for (const t of types) {
      const opt = document.createElement("option");
      opt.value = t.value;
      opt.textContent = t.label;
      opt.selected = action.type === t.value;
      typeSelect.appendChild(opt);
    }

    typeSelect.addEventListener("change", () => {
      const newType = typeSelect.value as WorkflowAction["type"];
      if (newType === "run_command") {
        step.actions[actionIndex] = { type: "run_command", command: "" };
      } else if (newType === "spawn_agent") {
        step.actions[actionIndex] = {
          type: "spawn_agent",
          prompt: "",
          name: "",
        };
      } else {
        step.actions[actionIndex] = {
          type: "run_workflow",
          workflowId: "",
        };
      }
      this.renderSteps();
    });

    actionHeader.appendChild(typeSelect);

    if (step.actions.length > 1) {
      const removeBtn = document.createElement("button");
      removeBtn.type = "button";
      removeBtn.className = "workflow-builder-action-remove";
      removeBtn.textContent = "\u2715";
      removeBtn.title = "Remove action";
      removeBtn.addEventListener("click", () => {
        step.actions.splice(actionIndex, 1);
        this.renderSteps();
      });
      actionHeader.appendChild(removeBtn);
    }

    wrapper.appendChild(actionHeader);

    const body = document.createElement("div");
    body.className = "workflow-builder-action-body";

    switch (action.type) {
      case "run_command": {
        const commandInput = document.createElement("input");
        commandInput.type = "text";
        commandInput.className = "workflow-builder-input";
        commandInput.placeholder = "e.g. git pull --rebase";
        commandInput.value = action.command;
        commandInput.addEventListener("input", () => {
          if (step.actions[actionIndex].type === "run_command") {
            (
              step.actions[actionIndex] as {
                type: "run_command";
                command: string;
              }
            ).command = commandInput.value;
          }
        });
        body.appendChild(commandInput);
        break;
      }
      case "spawn_agent": {
        const promptInput = document.createElement("textarea");
        promptInput.className = "workflow-builder-textarea";
        promptInput.placeholder =
          "Agent prompt (use {{previous_output}} for previous step output)";
        promptInput.value = action.prompt;
        promptInput.rows = 3;
        promptInput.addEventListener("input", () => {
          if (step.actions[actionIndex].type === "spawn_agent") {
            (
              step.actions[actionIndex] as {
                type: "spawn_agent";
                prompt: string;
                name: string;
              }
            ).prompt = promptInput.value;
          }
        });
        body.appendChild(promptInput);
        break;
      }
      case "run_workflow": {
        const workflows = this.store
          .getAll()
          .filter((w) => w.id !== this.editing?.id);
        const workflowSelect = document.createElement("select");
        workflowSelect.className = "workflow-builder-input";

        const emptyOpt = document.createElement("option");
        emptyOpt.value = "";
        emptyOpt.textContent = "Select a workflow...";
        workflowSelect.appendChild(emptyOpt);

        for (const w of workflows) {
          const opt = document.createElement("option");
          opt.value = w.id;
          opt.textContent = w.name;
          opt.selected = action.workflowId === w.id;
          workflowSelect.appendChild(opt);
        }

        workflowSelect.addEventListener("change", () => {
          if (step.actions[actionIndex].type === "run_workflow") {
            (
              step.actions[actionIndex] as {
                type: "run_workflow";
                workflowId: string;
              }
            ).workflowId = workflowSelect.value;
          }
        });
        body.appendChild(workflowSelect);
        break;
      }
    }

    wrapper.appendChild(body);
    return wrapper;
  }

  private async save(): Promise<void> {
    const name = this.nameInput.value.trim();
    const description = this.descriptionInput.value.trim();
    const trigger = this.triggerInput.value.trim();

    if (!name) {
      this.nameInput.focus();
      return;
    }
    if (!trigger || !trigger.startsWith("/")) {
      this.triggerInput.focus();
      return;
    }

    const id = this.editing?.id ?? generateId(name);

    const workflow: WorkflowDefinition = {
      id,
      name,
      description,
      trigger,
      steps: this.steps,
    };

    await this.store.save(workflow, this.scope);
    this.hide();
  }
}
