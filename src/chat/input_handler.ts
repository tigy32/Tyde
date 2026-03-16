import type { ImageAttachment } from "@tyde/protocol";
import { cancelConversation, sendMessage } from "../bridge";

export const SUPPORTED_IMAGE_EXTENSIONS = new Set([
  ".png",
  ".jpg",
  ".jpeg",
  ".gif",
  ".webp",
  ".svg",
  ".svgz",
  ".bmp",
]);

export interface InputHandlerCallbacks {
  getConversationId(): number | null;
  isDisconnected(): boolean;
  isTypingIndicatorVisible(): boolean;
  trackPendingUserEcho(text: string, images: ImageAttachment[]): string;
  removePendingUserEcho(fingerprint: string): void;
  openLightbox(src: string): void;
  onCancel(): void;
  onError(message: string): void;
  onReconnect(conversationId: number): void;
  onSystemMessage(text: string, style: "error"): void;
}

export class InputHistory {
  private history: string[];
  private historyIndex: number = -1;
  private savedDraft: string = "";

  constructor() {
    this.history = this.load();
  }

  push(text: string): void {
    // Most-recently-used ordering — displace earlier occurrence to avoid stale duplicates
    const idx = this.history.indexOf(text);
    if (idx !== -1) {
      this.history.splice(idx, 1);
    }
    this.history.unshift(text);
    if (this.history.length > 50) {
      this.history.length = 50;
    }
    this.save();
  }

  up(currentText: string): string | null {
    if (this.historyIndex + 1 >= this.history.length) return null;
    // Preserve draft so we can restore it when navigating back down
    if (this.historyIndex === -1) {
      this.savedDraft = currentText;
    }
    this.historyIndex++;
    return this.history[this.historyIndex];
  }

  down(): string | null {
    if (this.historyIndex <= -1) return null;
    this.historyIndex--;
    if (this.historyIndex === -1) return this.savedDraft;
    return this.history[this.historyIndex];
  }

  reset(): void {
    this.historyIndex = -1;
  }

  isBrowsing(): boolean {
    return this.historyIndex > -1;
  }

  private load(): string[] {
    const raw = localStorage.getItem("tyde-input-history");
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((s: unknown) => typeof s === "string").slice(0, 50);
  }

  private save(): void {
    try {
      localStorage.setItem("tyde-input-history", JSON.stringify(this.history));
    } catch (err) {
      console.error("Failed to save input history to localStorage:", err);
    }
  }
}

export class InputHandler {
  private inputArea: HTMLElement;
  private textarea: HTMLTextAreaElement;
  private sendBtn: HTMLButtonElement;
  private thumbnailRow: HTMLElement;
  private chatContainer: HTMLElement;
  private panelContainer: HTMLElement;
  private callbacks: InputHandlerCallbacks;
  private inputHistory = new InputHistory();
  private pendingImages: ImageAttachment[] = [];

  constructor(
    panelContainer: HTMLElement,
    chatContainer: HTMLElement,
    callbacks: InputHandlerCallbacks,
  ) {
    this.panelContainer = panelContainer;
    this.chatContainer = chatContainer;
    this.callbacks = callbacks;

    const { element, textarea, sendBtn, thumbnailRow } = this.buildInputArea();
    this.inputArea = element;
    this.textarea = textarea;
    this.sendBtn = sendBtn;
    this.thumbnailRow = thumbnailRow;

    this.attachDragDropHandlers();
  }

  getElement(): HTMLElement {
    return this.inputArea;
  }

  getTextarea(): HTMLTextAreaElement {
    return this.textarea;
  }

  focusInput(): void {
    this.textarea.focus();
  }

  updateSendButton(): void {
    this.sendBtn.disabled =
      this.callbacks.isDisconnected() ||
      this.callbacks.getConversationId() === null ||
      (!this.textarea.value.trim() && this.pendingImages.length === 0);
  }

  clearInput(): void {
    this.textarea.value = "";
    this.textarea.style.height = "auto";
    this.pendingImages = [];
    this.renderThumbnails();
    this.updateSendButton();
  }

  setWelcomeMode(_enabled: boolean): void {
    // Only handle input-specific reset; CSS class toggling stays in ChatPanel
    this.pendingImages = [];
    this.renderThumbnails();
    this.textarea.value = "";
    this.textarea.style.height = "auto";
  }

  private buildInputArea(): {
    element: HTMLElement;
    textarea: HTMLTextAreaElement;
    sendBtn: HTMLButtonElement;
    thumbnailRow: HTMLElement;
  } {
    const inputArea = document.createElement("div");
    inputArea.className = "input-area";

    const textarea = document.createElement("textarea");
    textarea.rows = 1;
    textarea.placeholder = "Type a message...";
    textarea.setAttribute("role", "textbox");
    textarea.setAttribute("aria-label", "Message input");

    const btnGroup = document.createElement("div");
    btnGroup.className = "input-buttons";

    const sendBtn = document.createElement("button") as HTMLButtonElement;
    sendBtn.className = "send-btn";
    sendBtn.textContent = "Send";
    sendBtn.setAttribute("aria-label", "Send message");

    const cancelBtn = document.createElement("button");
    cancelBtn.className = "cancel-btn";
    cancelBtn.textContent = "Interrupt";
    cancelBtn.setAttribute("aria-label", "Interrupt generation");

    const attachBtn = document.createElement("button");
    attachBtn.className = "attach-btn";
    attachBtn.textContent = "📎";
    attachBtn.setAttribute("aria-label", "Attach image");

    const fileInput = document.createElement("input");
    fileInput.type = "file";
    fileInput.accept =
      "image/png,image/jpeg,image/jpg,image/gif,image/webp,image/svg+xml";
    fileInput.multiple = true;
    fileInput.style.display = "none";
    fileInput.addEventListener("change", () => {
      if (!fileInput.files) return;
      for (const file of fileInput.files) {
        this.addImageFile(file);
      }
      fileInput.value = "";
    });
    attachBtn.addEventListener("click", () => fileInput.click());

    const thumbnailRow = document.createElement("div");
    thumbnailRow.className = "image-thumbnail-row hidden";

    btnGroup.appendChild(attachBtn);
    btnGroup.appendChild(sendBtn);
    btnGroup.appendChild(cancelBtn);
    inputArea.appendChild(textarea);
    inputArea.appendChild(thumbnailRow);
    inputArea.appendChild(fileInput);
    inputArea.appendChild(btnGroup);

    sendBtn.addEventListener("click", () => this.doSend());

    textarea.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && !e.shiftKey) {
        e.preventDefault();
        this.doSend();
        return;
      }
      if (e.key === "ArrowUp") {
        if (textarea.value !== "" && textarea.selectionStart !== 0) return;
        const entry = this.inputHistory.up(textarea.value);
        if (entry === null) return;
        e.preventDefault();
        textarea.value = entry;
        textarea.selectionStart = textarea.value.length;
        textarea.selectionEnd = textarea.value.length;
        return;
      }
      if (e.key === "ArrowDown") {
        if (!this.inputHistory.isBrowsing()) return;
        const entry = this.inputHistory.down();
        if (entry === null) return;
        e.preventDefault();
        textarea.value = entry;
        textarea.selectionStart = textarea.value.length;
        textarea.selectionEnd = textarea.value.length;
        return;
      }
      if (e.key === "Escape") {
        const convId = this.callbacks.getConversationId();
        if (convId === null || !this.callbacks.isTypingIndicatorVisible())
          return;
        cancelConversation(convId);
        return;
      }
    });

    textarea.addEventListener("input", () => {
      textarea.style.height = "auto";
      textarea.style.height = `${Math.min(textarea.scrollHeight, 120)}px`;
      textarea.style.overflowY =
        textarea.scrollHeight > 120 ? "auto" : "hidden";
      this.updateSendButton();
    });

    textarea.addEventListener("paste", (e) => {
      const items = e.clipboardData?.items;
      if (!items) return;
      for (const item of items) {
        if (!item.type.startsWith("image/")) continue;
        e.preventDefault();
        const file = item.getAsFile();
        if (file) this.addImageFile(file);
      }
    });

    cancelBtn.addEventListener("click", () => {
      this.callbacks.onCancel();
    });

    return { element: inputArea, textarea, sendBtn, thumbnailRow };
  }

  private async doSend(): Promise<void> {
    const text = this.textarea.value.trim();
    const convId = this.callbacks.getConversationId();
    if ((!text && this.pendingImages.length === 0) || convId === null) return;

    const optimisticImages = [...this.pendingImages];
    if (text) {
      this.inputHistory.push(text);
    }
    this.inputHistory.reset();

    const optimisticBubble = this.addUserBubble(text, optimisticImages);
    const fingerprint = this.callbacks.trackPendingUserEcho(
      text,
      optimisticImages,
    );
    const images = optimisticImages.length > 0 ? optimisticImages : undefined;

    this.textarea.value = "";
    this.textarea.style.height = "auto";
    this.pendingImages = [];
    this.renderThumbnails();
    this.updateSendButton();

    try {
      await sendMessage(convId, text, images);
    } catch (err: unknown) {
      this.callbacks.removePendingUserEcho(fingerprint);
      optimisticBubble.remove();
      const msg = err instanceof Error ? err.message : String(err);
      if (
        msg.includes("broken pipe") ||
        msg.includes("subprocess") ||
        msg.includes("not found")
      ) {
        this.callbacks.onReconnect(convId);
        return;
      }
      this.callbacks.onSystemMessage(msg, "error");
    }
  }

  private addUserBubble(text: string, images?: ImageAttachment[]): HTMLElement {
    const el = document.createElement("div");
    el.className = "message user-message";
    el.setAttribute("role", "article");
    if (text) {
      const textEl = document.createElement("div");
      textEl.textContent = text;
      el.appendChild(textEl);
    }
    if (images && images.length > 0) {
      const row = document.createElement("div");
      row.className = "message-images";
      for (const img of images) {
        const imgEl = document.createElement("img");
        imgEl.src = `data:${img.media_type};base64,${img.data}`;
        imgEl.alt = img.name;
        imgEl.className = "message-image-thumb";
        imgEl.addEventListener("click", () =>
          this.callbacks.openLightbox(imgEl.src),
        );
        row.appendChild(imgEl);
      }
      el.appendChild(row);
    }
    this.chatContainer.appendChild(el);
    this.scrollToBottom();
    return el;
  }

  private attachDragDropHandlers(): void {
    let dragDepth = 0;
    const inputArea = this.inputArea;

    this.panelContainer.addEventListener("dragenter", (e: DragEvent) => {
      if (!this.hasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      dragDepth += 1;
      inputArea.classList.add("drop-zone-active");
    });

    this.panelContainer.addEventListener("dragover", (e: DragEvent) => {
      if (!this.hasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
      inputArea.classList.add("drop-zone-active");
    });

    this.panelContainer.addEventListener("dragleave", (e: DragEvent) => {
      if (!this.hasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      dragDepth = Math.max(0, dragDepth - 1);
      if (dragDepth === 0) {
        inputArea.classList.remove("drop-zone-active");
      }
    });

    this.panelContainer.addEventListener("drop", (e: DragEvent) => {
      if (!this.hasFileDataTransfer(e.dataTransfer)) return;
      e.preventDefault();
      dragDepth = 0;
      inputArea.classList.remove("drop-zone-active");
      const result = this.addFilesFromDataTransfer(e.dataTransfer);
      if (result.hadFiles && result.added === 0) {
        this.callbacks.onError("Only image files can be attached");
      }
    });
  }

  private addImageFile(file: File): void {
    if (!isSupportedImageFile(file)) {
      this.callbacks.onError(`"${file.name}" is not a supported image type`);
      return;
    }

    const MAX_SIZE = 20 * 1024 * 1024;
    if (file.size > MAX_SIZE) {
      this.callbacks.onError(
        `Image "${file.name}" exceeds 20MB limit (${formatFileSize(file.size)})`,
      );
      return;
    }

    const reader = new FileReader();
    reader.onload = () => {
      const dataUrl = reader.result as string;
      const commaIdx = dataUrl.indexOf(",");
      const base64 = dataUrl.substring(commaIdx + 1);
      const attachment: ImageAttachment = {
        data: base64,
        media_type: file.type || "image/png",
        name: file.name || "pasted-image",
        size: file.size,
      };
      this.pendingImages.push(attachment);
      this.renderThumbnails();
      this.updateSendButton();
    };
    reader.readAsDataURL(file);
  }

  private hasFileDataTransfer(dataTransfer: DataTransfer | null): boolean {
    if (!dataTransfer) return false;
    if (dataTransfer.items && dataTransfer.items.length > 0) {
      for (const item of dataTransfer.items) {
        if (item.kind === "file") return true;
      }
    }
    return !!(dataTransfer.files && dataTransfer.files.length > 0);
  }

  private addFilesFromDataTransfer(dataTransfer: DataTransfer | null): {
    added: number;
    hadFiles: boolean;
  } {
    if (!dataTransfer) return { added: 0, hadFiles: false };

    const files: File[] = [];
    const seen = new Set<string>();
    let hadFiles = false;

    if (dataTransfer.items && dataTransfer.items.length > 0) {
      for (const item of dataTransfer.items) {
        if (item.kind !== "file") continue;
        hadFiles = true;
        const file = item.getAsFile();
        if (!file) continue;
        const key = `${file.name}|${file.size}|${file.type}|${file.lastModified}`;
        if (seen.has(key)) continue;
        seen.add(key);
        files.push(file);
      }
    }

    if (
      files.length === 0 &&
      dataTransfer.files &&
      dataTransfer.files.length > 0
    ) {
      hadFiles = true;
      for (const file of dataTransfer.files) {
        const key = `${file.name}|${file.size}|${file.type}|${file.lastModified}`;
        if (seen.has(key)) continue;
        seen.add(key);
        files.push(file);
      }
    }

    let added = 0;
    for (const file of files) {
      if (!isSupportedImageFile(file)) continue;
      this.addImageFile(file);
      added += 1;
    }

    return { added, hadFiles };
  }

  private renderThumbnails(): void {
    this.thumbnailRow.innerHTML = "";
    if (this.pendingImages.length === 0) {
      this.thumbnailRow.classList.add("hidden");
      return;
    }
    this.thumbnailRow.classList.remove("hidden");
    this.pendingImages.forEach((img, idx) => {
      const thumb = document.createElement("div");
      thumb.className = "image-thumbnail";
      thumb.style.opacity = "0";
      thumb.style.transform = "scale(0.8)";

      const imgEl = document.createElement("img");
      imgEl.src = `data:${img.media_type};base64,${img.data}`;
      imgEl.alt = img.name;

      const removeBtn = document.createElement("button");
      removeBtn.className = "image-thumbnail-remove";
      removeBtn.textContent = "×";
      removeBtn.addEventListener("click", () => {
        this.pendingImages.splice(idx, 1);
        this.renderThumbnails();
        this.updateSendButton();
      });

      const sizeBadge = document.createElement("span");
      sizeBadge.className = "image-thumbnail-size";
      sizeBadge.textContent = formatFileSize(img.size);

      thumb.appendChild(imgEl);
      thumb.appendChild(removeBtn);
      thumb.appendChild(sizeBadge);
      this.thumbnailRow.appendChild(thumb);

      requestAnimationFrame(() => {
        thumb.style.opacity = "1";
        thumb.style.transform = "scale(1)";
      });
    });
  }

  private scrollToBottom(): void {
    this.chatContainer.scrollTop = this.chatContainer.scrollHeight;
  }
}

function isSupportedImageFile(file: File): boolean {
  if (file.type?.startsWith("image/")) return true;
  const dot = file.name.lastIndexOf(".");
  if (dot === -1) return false;
  const ext = file.name.substring(dot).toLowerCase();
  return SUPPORTED_IMAGE_EXTENSIONS.has(ext);
}

function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}
