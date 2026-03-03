export interface TextPromptOptions {
  title: string;
  description?: string;
  defaultValue?: string;
  placeholder?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  validate?: (value: string) => string | null;
}

export function promptForText(
  options: TextPromptOptions,
): Promise<string | null> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "text-prompt-overlay";
    overlay.dataset.testid = "text-prompt";

    const card = document.createElement("div");
    card.className = "text-prompt-card";
    card.setAttribute("role", "dialog");
    card.setAttribute("aria-modal", "true");
    card.setAttribute("aria-label", options.title);

    const title = document.createElement("h3");
    title.className = "text-prompt-title";
    title.textContent = options.title;
    card.appendChild(title);

    if (options.description) {
      const description = document.createElement("p");
      description.className = "text-prompt-description";
      description.textContent = options.description;
      card.appendChild(description);
    }

    const input = document.createElement("input");
    input.className = "text-prompt-input";
    input.dataset.testid = "text-prompt-input";
    input.type = "text";
    input.value = options.defaultValue ?? "";
    input.placeholder = options.placeholder ?? "";
    card.appendChild(input);

    const error = document.createElement("div");
    error.className = "text-prompt-error";
    error.style.display = "none";
    card.appendChild(error);

    const actions = document.createElement("div");
    actions.className = "text-prompt-actions";

    const cancelBtn = document.createElement("button");
    cancelBtn.type = "button";
    cancelBtn.className = "text-prompt-btn";
    cancelBtn.textContent = options.cancelLabel ?? "Cancel";

    const confirmBtn = document.createElement("button");
    confirmBtn.type = "button";
    confirmBtn.className = "text-prompt-btn text-prompt-btn-primary";
    confirmBtn.dataset.testid = "text-prompt-confirm";
    confirmBtn.textContent = options.confirmLabel ?? "OK";

    actions.appendChild(cancelBtn);
    actions.appendChild(confirmBtn);
    card.appendChild(actions);
    overlay.appendChild(card);

    let settled = false;
    const finish = (value: string | null) => {
      if (settled) return;
      settled = true;
      overlay.remove();
      resolve(value);
    };

    const submit = () => {
      const value = input.value;
      const validationError = options.validate?.(value) ?? null;
      if (validationError) {
        error.textContent = validationError;
        error.style.display = "block";
        input.focus();
        return;
      }
      finish(value);
    };

    confirmBtn.addEventListener("click", submit);
    cancelBtn.addEventListener("click", () => finish(null));
    overlay.addEventListener("click", (event) => {
      if (event.target === overlay) finish(null);
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        finish(null);
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        submit();
      }
    });
    card.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        finish(null);
      }
    });

    document.body.appendChild(overlay);
    requestAnimationFrame(() => {
      input.focus();
      input.select();
    });
  });
}
