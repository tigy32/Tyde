import { listen, type UnlistenFn } from "@tauri-apps/api/event";

interface RemoteConnectionProgress {
  host: string;
  step: string;
  status: string;
  message: string;
}

interface StepState {
  label: string;
  status: "pending" | "in_progress" | "completed" | "skipped" | "failed";
  message: string;
}

const STEP_LABELS: Record<string, string> = {
  validating_connection: "Validate SSH Connection",
  checking_environment: "Check Remote Environment",
  installing_subprocess: "Install Tyde",
  ready: "Connected",
};

const STEP_ORDER = [
  "validating_connection",
  "checking_environment",
  "installing_subprocess",
  "ready",
];

export class ConnectionDialog {
  private overlay: HTMLElement | null = null;
  private steps = new Map<string, StepState>();
  private unlisten: UnlistenFn | null = null;
  private dismissTimer: number | null = null;
  private onDismiss: (() => void) | null = null;

  async show(host: string, onDismiss?: () => void): Promise<void> {
    this.onDismiss = onDismiss ?? null;
    this.steps.clear();
    for (const key of STEP_ORDER) {
      this.steps.set(key, {
        label: STEP_LABELS[key] ?? key,
        status: "pending",
        message: "",
      });
    }

    this.buildDOM(host);

    this.unlisten = await listen<RemoteConnectionProgress>(
      "remote-connection-progress",
      (event) => {
        const p = event.payload;
        if (p.host !== host) return;
        this.updateStep(p.step, p.status as StepState["status"], p.message);
      },
    );
  }

  dismiss(): void {
    if (this.dismissTimer !== null) {
      clearTimeout(this.dismissTimer);
      this.dismissTimer = null;
    }
    this.unlisten?.();
    this.unlisten = null;
    if (this.overlay) {
      this.overlay.classList.add("conn-dialog-exit");
      const el = this.overlay;
      setTimeout(() => el.remove(), 200);
      this.overlay = null;
    }
    this.onDismiss?.();
  }

  private buildDOM(host: string): void {
    if (this.overlay) this.overlay.remove();

    this.overlay = document.createElement("div");
    this.overlay.className = "conn-dialog-overlay";
    this.overlay.dataset.testid = "connection-dialog";

    const card = document.createElement("div");
    card.className = "conn-dialog-card";

    const title = document.createElement("h3");
    title.className = "conn-dialog-title";
    title.textContent = "Connecting to Remote";

    const hostLabel = document.createElement("div");
    hostLabel.className = "conn-dialog-host";
    hostLabel.dataset.testid = "connection-host";
    hostLabel.textContent = host;

    const stepList = document.createElement("div");
    stepList.className = "conn-dialog-steps";

    for (const key of STEP_ORDER) {
      const state = this.steps.get(key)!;
      const row = document.createElement("div");
      row.className = "conn-dialog-step";
      row.dataset.step = key;

      const indicator = document.createElement("div");
      indicator.className = "conn-dialog-indicator pending";

      const content = document.createElement("div");
      content.className = "conn-dialog-step-content";

      const label = document.createElement("div");
      label.className = "conn-dialog-step-label";
      label.textContent = state.label;

      const msg = document.createElement("div");
      msg.className = "conn-dialog-step-message";

      content.appendChild(label);
      content.appendChild(msg);
      row.appendChild(indicator);
      row.appendChild(content);
      stepList.appendChild(row);
    }

    card.appendChild(title);
    card.appendChild(hostLabel);
    card.appendChild(stepList);

    const closeBtn = document.createElement("button");
    closeBtn.className = "conn-dialog-close-btn hidden";
    closeBtn.dataset.testid = "connection-close";
    closeBtn.textContent = "Close";
    closeBtn.addEventListener("click", () => this.dismiss());
    card.appendChild(closeBtn);

    this.overlay.appendChild(card);
    document.body.appendChild(this.overlay);
  }

  private updateStep(
    step: string,
    status: StepState["status"],
    message: string,
  ): void {
    const state = this.steps.get(step);
    if (state) {
      state.status = status;
      state.message = message;
    }

    if (!this.overlay) return;

    const row = this.overlay.querySelector(`[data-step="${step}"]`);
    if (!row) return;

    const indicator = row.querySelector(".conn-dialog-indicator")!;
    indicator.className = `conn-dialog-indicator ${status}`;

    const msgEl = row.querySelector(".conn-dialog-step-message")!;
    msgEl.textContent = message;

    if (step === "ready" && status === "completed") {
      this.dismissTimer = window.setTimeout(() => this.dismiss(), 1500);
    }

    if (status === "failed") {
      const closeBtn = this.overlay.querySelector(".conn-dialog-close-btn");
      closeBtn?.classList.remove("hidden");
    }
  }
}
