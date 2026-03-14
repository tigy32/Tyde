import {
  type BackendKind,
  getSettings,
  listModels,
  listProfiles,
  sendMessage,
  switchProfile,
  updateSettings,
} from "../bridge";

export interface SessionSettingsHandle {
  element: HTMLElement;
  updateSettings(data: any): void;
  updateProfiles(data: { profiles: string[]; active_profile?: string }): void;
  updateModels(data: {
    models: Array<{ id: string; displayName: string; isDefault: boolean }>;
  }): void;
  setBackendKind(kind: BackendKind): void;
}

export function createSessionSettings(
  conversationId: number,
  initialBackendKind: BackendKind = "tycode",
): SessionSettingsHandle {
  const autonomyLabels: Record<number, string> = {
    1: "Plan Approval",
    2: "Fully Autonomous",
  };
  const autonomyValues: Record<number, string> = {
    1: "plan_approval_required",
    2: "fully_autonomous",
  };
  const orchLabels: Record<number, string> = {
    1: "None",
    2: "Auto",
    3: "Required",
  };
  const orchCommands: Record<number, string> = {
    1: "/agent one_shot",
    2: "/agent tycode",
    3: "/agent coordinator",
  };
  const reasoningLabels: Record<number, string> = {
    0: "Off",
    1: "Low",
    2: "Medium",
    3: "High",
    4: "Max",
  };
  const reasoningValues = ["Off", "Low", "Medium", "High", "Max"];

  const codexReasoningLabels: Record<number, string> = {
    0: "Low",
    1: "Medium",
    2: "High",
    3: "Extra High",
  };
  const codexReasoningValues = ["low", "medium", "high", "xhigh"];
  const claudeEffortLabels: Record<number, string> = {
    0: "Low",
    1: "Medium",
    2: "High",
    3: "Max",
  };
  const claudeEffortValues = ["low", "medium", "high", "max"];

  let backendKind: BackendKind = initialBackendKind;
  let currentAutonomy = "plan_approval_required";
  let currentReasoning = "Medium";
  let knownActiveProfile = "";

  let codexReasoningEffort = "xhigh";
  let codexModel: string | null = null;
  let _codexModelsLoaded = false;

  let claudeEffort = "high";
  let claudeModel: string | null = null;
  let _claudeModelsLoaded = false;

  let kiroModel: string | null = null;
  let _kiroModelsLoaded = false;

  const panel = document.createElement("div");
  panel.className = "session-settings-panel";
  panel.dataset.testid = "session-settings-panel";

  const toggle = document.createElement("div");
  toggle.className = "session-settings-toggle";
  toggle.dataset.testid = "session-settings-toggle";

  const chevron = document.createElement("span");
  chevron.className = "session-settings-chevron";
  chevron.textContent = "▲";

  const toggleLabel = document.createElement("span");
  toggleLabel.textContent = "Session Settings";

  toggle.appendChild(chevron);
  toggle.appendChild(toggleLabel);

  const content = document.createElement("div");
  content.className = "session-settings-content";
  content.style.display = "none";

  const tycodeGrid = document.createElement("div");
  tycodeGrid.className = "session-settings-grid";

  const codexGrid = document.createElement("div");
  codexGrid.className = "session-settings-grid";

  const claudeGrid = document.createElement("div");
  claudeGrid.className = "session-settings-grid";

  const kiroGrid = document.createElement("div");
  kiroGrid.className = "session-settings-grid";

  toggle.addEventListener("click", () => {
    const hidden = content.style.display === "none";
    content.style.display = hidden ? "" : "none";
    chevron.textContent = hidden ? "▼" : "▲";
    if (!hidden) return;
    getSettings(conversationId).catch((err) =>
      console.error("Failed to get settings on toggle:", err),
    );
    if (backendKind === "tycode") {
      listProfiles(conversationId).catch((err) =>
        console.error("Failed to list profiles on toggle:", err),
      );
    }
    if (
      backendKind === "codex" ||
      backendKind === "claude" ||
      backendKind === "kiro"
    ) {
      listModels(conversationId).catch((err) =>
        console.error("Failed to list models on toggle:", err),
      );
    }
  });

  const autonomyRow = document.createElement("div");
  autonomyRow.className = "session-settings-row";

  const autonomyLabel = document.createElement("label");
  autonomyLabel.className = "session-settings-label";
  autonomyLabel.textContent = "Autonomy";

  const autonomySlider = document.createElement("input");
  autonomySlider.type = "range";
  autonomySlider.className = "session-settings-slider";
  autonomySlider.min = "1";
  autonomySlider.max = "2";
  autonomySlider.value = "1";

  const autonomyValue = document.createElement("span");
  autonomyValue.className = "session-settings-value";
  autonomyValue.textContent = autonomyLabels[1];

  autonomySlider.addEventListener("input", () => {
    const pos = Number(autonomySlider.value);
    autonomyValue.textContent = autonomyLabels[pos];
    currentAutonomy = autonomyValues[pos];
    updateSettings(conversationId, { autonomy_level: currentAutonomy });
  });

  autonomyRow.appendChild(autonomyLabel);
  autonomyRow.appendChild(autonomySlider);
  autonomyRow.appendChild(autonomyValue);

  const orchRow = document.createElement("div");
  orchRow.className = "session-settings-row";

  const orchLabel = document.createElement("label");
  orchLabel.className = "session-settings-label";
  orchLabel.textContent = "Orchestration";

  const orchSlider = document.createElement("input");
  orchSlider.type = "range";
  orchSlider.className = "session-settings-slider";
  orchSlider.min = "1";
  orchSlider.max = "3";
  orchSlider.value = "2";

  const orchValue = document.createElement("span");
  orchValue.className = "session-settings-value";
  orchValue.textContent = orchLabels[2];

  orchSlider.addEventListener("input", () => {
    const pos = Number(orchSlider.value);
    orchValue.textContent = orchLabels[pos];
    sendMessage(conversationId, orchCommands[pos]);
  });

  orchRow.appendChild(orchLabel);
  orchRow.appendChild(orchSlider);
  orchRow.appendChild(orchValue);

  const reasoningRow = document.createElement("div");
  reasoningRow.className = "session-settings-row";

  const reasoningLabel = document.createElement("label");
  reasoningLabel.className = "session-settings-label";
  reasoningLabel.textContent = "Reasoning";

  const reasoningSlider = document.createElement("input");
  reasoningSlider.type = "range";
  reasoningSlider.className = "session-settings-slider";
  reasoningSlider.min = "0";
  reasoningSlider.max = "4";
  reasoningSlider.value = "2";

  const reasoningValue = document.createElement("span");
  reasoningValue.className = "session-settings-value";
  reasoningValue.textContent = reasoningLabels[2];

  reasoningSlider.addEventListener("input", () => {
    const pos = Number(reasoningSlider.value);
    reasoningValue.textContent = reasoningLabels[pos];
    currentReasoning = reasoningValues[pos];
    updateSettings(conversationId, { reasoning_effort: currentReasoning });
  });

  reasoningRow.appendChild(reasoningLabel);
  reasoningRow.appendChild(reasoningSlider);
  reasoningRow.appendChild(reasoningValue);

  const profileRow = document.createElement("div");
  profileRow.className = "session-settings-row";

  const profileLabel = document.createElement("label");
  profileLabel.className = "session-settings-label";
  profileLabel.textContent = "Profile";

  const profileInner = document.createElement("div");
  profileInner.className = "session-settings-profile-row";

  const profileSelect = document.createElement("select");
  profileSelect.className = "session-profile-select";

  profileSelect.addEventListener("change", () => {
    switchProfile(conversationId, profileSelect.value);
  });

  const refreshBtn = document.createElement("button");
  refreshBtn.className = "session-settings-btn";
  refreshBtn.title = "Refresh profiles";
  refreshBtn.textContent = "↻";
  refreshBtn.addEventListener("click", () => {
    listProfiles(conversationId);
  });

  const saveBtn = document.createElement("button");
  saveBtn.className = "session-settings-btn";
  saveBtn.title = "Save session defaults";
  saveBtn.textContent = "💾";
  saveBtn.addEventListener("click", () => {
    updateSettings(conversationId, {
      autonomy_level: currentAutonomy,
      reasoning_effort: currentReasoning,
    });
  });

  profileInner.appendChild(profileSelect);
  profileInner.appendChild(refreshBtn);
  profileInner.appendChild(saveBtn);

  profileRow.appendChild(profileLabel);
  profileRow.appendChild(profileInner);

  tycodeGrid.appendChild(autonomyRow);
  tycodeGrid.appendChild(orchRow);
  tycodeGrid.appendChild(reasoningRow);
  tycodeGrid.appendChild(profileRow);

  // --- Codex settings ---

  const codexReasoningRow = document.createElement("div");
  codexReasoningRow.className = "session-settings-row";

  const codexReasoningLabel = document.createElement("label");
  codexReasoningLabel.className = "session-settings-label";
  codexReasoningLabel.textContent = "Reasoning Effort";

  const codexReasoningSlider = document.createElement("input");
  codexReasoningSlider.type = "range";
  codexReasoningSlider.className = "session-settings-slider";
  codexReasoningSlider.min = "0";
  codexReasoningSlider.max = "3";
  codexReasoningSlider.value = "3";

  const codexReasoningValue = document.createElement("span");
  codexReasoningValue.className = "session-settings-value";
  codexReasoningValue.textContent = codexReasoningLabels[3];

  codexReasoningSlider.addEventListener("input", () => {
    const pos = Number(codexReasoningSlider.value);
    codexReasoningValue.textContent = codexReasoningLabels[pos];
    codexReasoningEffort = codexReasoningValues[pos];
    updateSettings(conversationId, { reasoning_effort: codexReasoningEffort });
  });

  codexReasoningRow.appendChild(codexReasoningLabel);
  codexReasoningRow.appendChild(codexReasoningSlider);
  codexReasoningRow.appendChild(codexReasoningValue);

  const codexModelRow = document.createElement("div");
  codexModelRow.className = "session-settings-row";

  const codexModelLabel = document.createElement("label");
  codexModelLabel.className = "session-settings-label";
  codexModelLabel.textContent = "Model";

  const codexModelSelect = document.createElement("select");
  codexModelSelect.className = "session-profile-select";

  // Placeholder option before models are loaded
  const codexDefaultOpt = document.createElement("option");
  codexDefaultOpt.value = "";
  codexDefaultOpt.textContent = "Default";
  codexModelSelect.appendChild(codexDefaultOpt);

  codexModelSelect.addEventListener("change", () => {
    const next = codexModelSelect.value;
    codexModel = next.length > 0 ? next : null;
    updateSettings(conversationId, { model: codexModel });
  });

  codexModelRow.appendChild(codexModelLabel);
  codexModelRow.appendChild(codexModelSelect);

  codexGrid.appendChild(codexReasoningRow);
  codexGrid.appendChild(codexModelRow);

  // --- Claude settings ---

  const claudeEffortRow = document.createElement("div");
  claudeEffortRow.className = "session-settings-row";

  const claudeEffortLabel = document.createElement("label");
  claudeEffortLabel.className = "session-settings-label";
  claudeEffortLabel.textContent = "Effort";

  const claudeEffortSlider = document.createElement("input");
  claudeEffortSlider.type = "range";
  claudeEffortSlider.className = "session-settings-slider";
  claudeEffortSlider.min = "0";
  claudeEffortSlider.max = "3";
  claudeEffortSlider.value = "2";

  const claudeEffortValue = document.createElement("span");
  claudeEffortValue.className = "session-settings-value";
  claudeEffortValue.textContent = claudeEffortLabels[2];

  claudeEffortSlider.addEventListener("input", () => {
    const pos = Number(claudeEffortSlider.value);
    claudeEffort = claudeEffortValues[pos];
    claudeEffortValue.textContent = claudeEffortLabels[pos];
    updateSettings(conversationId, { effort: claudeEffort });
  });

  claudeEffortRow.appendChild(claudeEffortLabel);
  claudeEffortRow.appendChild(claudeEffortSlider);
  claudeEffortRow.appendChild(claudeEffortValue);

  const claudeModelRow = document.createElement("div");
  claudeModelRow.className = "session-settings-row";

  const claudeModelLabel = document.createElement("label");
  claudeModelLabel.className = "session-settings-label";
  claudeModelLabel.textContent = "Model";

  const claudeModelSelect = document.createElement("select");
  claudeModelSelect.className = "session-profile-select";

  // Placeholder option before models are loaded
  const claudeDefaultOpt = document.createElement("option");
  claudeDefaultOpt.value = "";
  claudeDefaultOpt.textContent = "Default";
  claudeModelSelect.appendChild(claudeDefaultOpt);

  claudeModelSelect.addEventListener("change", () => {
    const next = claudeModelSelect.value;
    claudeModel = next.length > 0 ? next : null;
    updateSettings(conversationId, { model: claudeModel });
  });

  claudeModelRow.appendChild(claudeModelLabel);
  claudeModelRow.appendChild(claudeModelSelect);

  claudeGrid.appendChild(claudeEffortRow);
  claudeGrid.appendChild(claudeModelRow);

  // --- Kiro settings ---

  const kiroModelRow = document.createElement("div");
  kiroModelRow.className = "session-settings-row";

  const kiroModelLabel = document.createElement("label");
  kiroModelLabel.className = "session-settings-label";
  kiroModelLabel.textContent = "Model";

  const kiroModelSelect = document.createElement("select");
  kiroModelSelect.className = "session-profile-select";

  const kiroDefaultOpt = document.createElement("option");
  kiroDefaultOpt.value = "";
  kiroDefaultOpt.textContent = "Default";
  kiroModelSelect.appendChild(kiroDefaultOpt);

  kiroModelSelect.addEventListener("change", () => {
    const next = kiroModelSelect.value;
    kiroModel = next.length > 0 ? next : null;
    updateSettings(conversationId, { model: kiroModel });
  });

  kiroModelRow.appendChild(kiroModelLabel);
  kiroModelRow.appendChild(kiroModelSelect);

  kiroGrid.appendChild(kiroModelRow);

  content.appendChild(tycodeGrid);
  content.appendChild(codexGrid);
  content.appendChild(claudeGrid);
  content.appendChild(kiroGrid);

  panel.appendChild(toggle);
  panel.appendChild(content);

  const populateModelSelect = (
    select: HTMLElement,
    models: Array<{ id: string; displayName: string; isDefault: boolean }>,
    currentModel: string | null,
  ) => {
    const selectEl = select as HTMLSelectElement;
    selectEl.innerHTML = "";

    const defaultOpt = document.createElement("option");
    defaultOpt.value = "";
    defaultOpt.textContent = "Default";
    if (!currentModel) defaultOpt.selected = true;
    selectEl.appendChild(defaultOpt);

    for (const model of models) {
      const opt = document.createElement("option");
      opt.value = model.id;
      opt.textContent = model.displayName;
      if (currentModel === model.id) opt.selected = true;
      selectEl.appendChild(opt);
    }
  };

  const syncBackendVisibility = () => {
    toggleLabel.textContent =
      backendKind === "codex"
        ? "Session Settings (Codex)"
        : backendKind === "claude"
          ? "Session Settings (Claude)"
          : backendKind === "kiro"
            ? "Session Settings (Kiro)"
            : "Session Settings";
    tycodeGrid.style.display = backendKind === "tycode" ? "" : "none";
    codexGrid.style.display = backendKind === "codex" ? "" : "none";
    claudeGrid.style.display = backendKind === "claude" ? "" : "none";
    kiroGrid.style.display = backendKind === "kiro" ? "" : "none";
  };
  syncBackendVisibility();

  return {
    element: panel,

    updateSettings(data: any) {
      if (!data || typeof data !== "object") return;

      if (backendKind === "codex") {
        const codexEffortRaw = data.reasoning_effort ?? data.reasoningEffort;
        if (typeof codexEffortRaw === "string") {
          const normalized = codexEffortRaw.trim().toLowerCase();
          const idx = codexReasoningValues.indexOf(normalized);
          const mapped =
            idx >= 0
              ? idx
              : normalized === "none" || normalized === "minimal"
                ? 0
                : 3;
          codexReasoningEffort = codexReasoningValues[mapped];
          codexReasoningSlider.value = String(mapped);
          codexReasoningValue.textContent = codexReasoningLabels[mapped];
        }

        const modelRaw = data.model;
        if (typeof modelRaw === "string") {
          codexModel = modelRaw.trim().length > 0 ? modelRaw.trim() : null;
          codexModelSelect.value = codexModel ?? "";
        } else if (modelRaw === null) {
          codexModel = null;
          codexModelSelect.value = "";
        }
        return;
      }

      if (backendKind === "claude") {
        const claudeEffortRaw =
          data.effort ?? data.reasoning_effort ?? data.reasoningEffort;
        if (typeof claudeEffortRaw === "string") {
          const normalized = claudeEffortRaw.trim().toLowerCase();
          const idx = claudeEffortValues.indexOf(normalized);
          const mapped = idx >= 0 ? idx : 2;
          claudeEffort = claudeEffortValues[mapped];
          claudeEffortSlider.value = String(mapped);
          claudeEffortValue.textContent = claudeEffortLabels[mapped];
        }

        const modelRaw = data.model;
        if (typeof modelRaw === "string") {
          claudeModel = modelRaw.trim().length > 0 ? modelRaw.trim() : null;
          claudeModelSelect.value = claudeModel ?? "";
        } else if (modelRaw === null) {
          claudeModel = null;
          claudeModelSelect.value = "";
        }
        return;
      }

      if (backendKind === "kiro") {
        const modelRaw = data.model;
        if (typeof modelRaw === "string") {
          kiroModel = modelRaw.trim().length > 0 ? modelRaw.trim() : null;
          kiroModelSelect.value = kiroModel ?? "";
        } else if (modelRaw === null) {
          kiroModel = null;
          kiroModelSelect.value = "";
        }
        return;
      }

      if (data.autonomy_level) {
        currentAutonomy = data.autonomy_level;
        const isAuto =
          data.autonomy_level === "fully_autonomous" ||
          data.autonomy_level === "FullyAutonomous";
        const pos = isAuto ? 2 : 1;
        autonomySlider.value = String(pos);
        autonomyValue.textContent = autonomyLabels[pos];
      }

      if (data.default_agent) {
        let pos = 2;
        if (data.default_agent === "one_shot") pos = 1;
        if (data.default_agent === "coordinator") pos = 3;
        orchSlider.value = String(pos);
        orchValue.textContent = orchLabels[pos];
      }

      if (data.reasoning_effort) {
        currentReasoning = data.reasoning_effort;
        const idx = reasoningValues.indexOf(data.reasoning_effort);
        const pos = idx >= 0 ? idx : 2;
        reasoningSlider.value = String(pos);
        reasoningValue.textContent = reasoningLabels[pos];
      }

      const profile = data.profile ?? data.active_profile;
      if (typeof profile === "string" && profile.length > 0) {
        knownActiveProfile = profile;
        for (const opt of profileSelect.options) {
          opt.selected = opt.value === profile;
        }
      }
    },

    updateProfiles(data: { profiles: string[]; active_profile?: string }) {
      if (backendKind !== "tycode") return;
      profileSelect.innerHTML = "";
      const activeProfile = data.active_profile ?? knownActiveProfile;

      for (const name of data.profiles) {
        const opt = document.createElement("option");
        opt.value = name;
        opt.textContent = name;
        if (name === activeProfile) opt.selected = true;
        profileSelect.appendChild(opt);
      }
    },

    updateModels(data: {
      models: Array<{ id: string; displayName: string; isDefault: boolean }>;
    }) {
      if (!data || !Array.isArray(data.models)) return;

      if (backendKind === "codex") {
        _codexModelsLoaded = true;
        populateModelSelect(codexModelSelect, data.models, codexModel);
      } else if (backendKind === "claude") {
        _claudeModelsLoaded = true;
        populateModelSelect(claudeModelSelect, data.models, claudeModel);
      } else if (backendKind === "kiro") {
        _kiroModelsLoaded = true;
        populateModelSelect(kiroModelSelect, data.models, kiroModel);
      }
    },

    setBackendKind(kind: BackendKind) {
      backendKind = kind;
      syncBackendVisibility();
      if (content.style.display !== "none") {
        getSettings(conversationId).catch((err) =>
          console.error("Failed to get settings on backend switch:", err),
        );
        if (backendKind === "tycode") {
          listProfiles(conversationId).catch((err) =>
            console.error("Failed to list profiles on backend switch:", err),
          );
        }
        if (
          backendKind === "codex" ||
          backendKind === "claude" ||
          backendKind === "kiro"
        ) {
          listModels(conversationId).catch((err) =>
            console.error("Failed to list models on backend switch:", err),
          );
        }
      }
    },
  };
}
