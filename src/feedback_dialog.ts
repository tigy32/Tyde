import { invoke } from "@tauri-apps/api/core";

export function showFeedbackDialog(): void {
  const overlay = document.createElement("div");
  overlay.className = "text-prompt-overlay";
  overlay.dataset.testid = "feedback-dialog";

  const card = document.createElement("div");
  card.className = "text-prompt-card";
  card.setAttribute("role", "dialog");
  card.setAttribute("aria-modal", "true");
  card.setAttribute("aria-label", "Send Feedback");

  const title = document.createElement("h3");
  title.className = "text-prompt-title";
  title.textContent = "Send Feedback";
  card.appendChild(title);

  const description = document.createElement("p");
  description.className = "text-prompt-description";
  description.textContent =
    "Let us know what you think — bugs, ideas, anything.";
  card.appendChild(description);

  const textarea = document.createElement("textarea");
  textarea.className = "text-prompt-input feedback-textarea";
  textarea.dataset.testid = "feedback-textarea";
  textarea.placeholder = "Your feedback…";
  textarea.rows = 5;
  card.appendChild(textarea);

  const status = document.createElement("div");
  status.className = "feedback-status";
  status.style.display = "none";
  card.appendChild(status);

  const actions = document.createElement("div");
  actions.className = "text-prompt-actions";

  const cancelBtn = document.createElement("button");
  cancelBtn.type = "button";
  cancelBtn.className = "text-prompt-btn";
  cancelBtn.textContent = "Cancel";

  const submitBtn = document.createElement("button");
  submitBtn.type = "button";
  submitBtn.className = "text-prompt-btn text-prompt-btn-primary";
  submitBtn.dataset.testid = "feedback-submit";
  submitBtn.textContent = "Send";

  actions.appendChild(cancelBtn);
  actions.appendChild(submitBtn);
  card.appendChild(actions);
  overlay.appendChild(card);

  const dismiss = () => overlay.remove();

  const submit = async () => {
    const text = textarea.value.trim();
    if (!text) {
      status.textContent = "Please enter some feedback.";
      status.className = "feedback-status feedback-status-error";
      status.style.display = "block";
      textarea.focus();
      return;
    }

    submitBtn.disabled = true;
    submitBtn.textContent = "Sending…";

    try {
      await invoke("submit_feedback", { feedback: text });
      status.textContent = "Thanks for your feedback!";
      status.className = "feedback-status feedback-status-success";
      status.style.display = "block";
      submitBtn.style.display = "none";
      cancelBtn.textContent = "Close";
      textarea.disabled = true;
    } catch (err) {
      status.textContent = String(err);
      status.className = "feedback-status feedback-status-error";
      status.style.display = "block";
      submitBtn.disabled = false;
      submitBtn.textContent = "Send";
    }
  };

  submitBtn.addEventListener("click", () => void submit());
  cancelBtn.addEventListener("click", dismiss);
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) dismiss();
  });
  card.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      dismiss();
    }
  });

  document.body.appendChild(overlay);
  requestAnimationFrame(() => textarea.focus());
}
