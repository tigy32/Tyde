import type { ImageAttachment } from "../types";
import { SUPPORTED_IMAGE_EXTENSIONS } from "./input_handler";

export interface InputState {
  pendingImages: ImageAttachment[];
  thumbnailRow: HTMLElement | null;
  inputArea: HTMLElement | null;
  sendBtn: HTMLButtonElement | null;
}

export function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
}

export function isSupportedImageFile(file: File): boolean {
  if (file.type?.startsWith("image/")) return true;
  const dot = file.name.lastIndexOf(".");
  if (dot === -1) return false;
  const ext = file.name.substring(dot).toLowerCase();
  return SUPPORTED_IMAGE_EXTENSIONS.has(ext);
}

export function hasFileDataTransfer(
  dataTransfer: DataTransfer | null,
): boolean {
  if (!dataTransfer) return false;
  if (dataTransfer.items && dataTransfer.items.length > 0) {
    for (const item of dataTransfer.items) {
      if (item.kind === "file") return true;
    }
  }
  return !!(dataTransfer.files && dataTransfer.files.length > 0);
}

export function addImageFile(
  state: InputState,
  file: File,
  notifyError: (msg: string) => void,
  updateSendButton: () => void,
): void {
  if (!isSupportedImageFile(file)) {
    notifyError(`"${file.name}" is not a supported image type`);
    return;
  }

  const MAX_SIZE = 20 * 1024 * 1024;
  if (file.size > MAX_SIZE) {
    notifyError(
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
    state.pendingImages.push(attachment);
    renderThumbnails(state, updateSendButton);
    updateSendButton();
  };
  reader.readAsDataURL(file);
}

export function addFilesFromDataTransfer(
  state: InputState,
  dataTransfer: DataTransfer | null,
  notifyError: (msg: string) => void,
  updateSendButton: () => void,
): { added: number; hadFiles: boolean } {
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
    addImageFile(state, file, notifyError, updateSendButton);
    added += 1;
  }

  return { added, hadFiles };
}

export function renderThumbnails(
  state: InputState,
  updateSendButton: () => void,
): void {
  if (!state.thumbnailRow) return;
  state.thumbnailRow.innerHTML = "";
  if (state.pendingImages.length === 0) {
    state.thumbnailRow.classList.add("hidden");
    return;
  }
  state.thumbnailRow.classList.remove("hidden");
  state.pendingImages.forEach((img, idx) => {
    const thumb = document.createElement("div");
    thumb.className = "image-thumbnail";
    thumb.style.opacity = "0";
    thumb.style.transform = "scale(0.8)";

    const imgEl = document.createElement("img");
    imgEl.src = `data:${img.media_type};base64,${img.data}`;
    imgEl.alt = img.name;

    const removeBtn = document.createElement("button");
    removeBtn.className = "image-thumbnail-remove";
    removeBtn.textContent = "\u00d7";
    removeBtn.addEventListener("click", () => {
      state.pendingImages.splice(idx, 1);
      renderThumbnails(state, updateSendButton);
      updateSendButton();
    });

    const sizeBadge = document.createElement("span");
    sizeBadge.className = "image-thumbnail-size";
    sizeBadge.textContent = formatFileSize(img.size);

    thumb.appendChild(imgEl);
    thumb.appendChild(removeBtn);
    thumb.appendChild(sizeBadge);
    state.thumbnailRow!.appendChild(thumb);

    requestAnimationFrame(() => {
      thumb.style.opacity = "1";
      thumb.style.transform = "scale(1)";
    });
  });
}

export function openLightbox(src: string): void {
  const overlay = document.createElement("div");
  overlay.className = "lightbox-overlay";

  const img = document.createElement("img");
  img.className = "lightbox-image";
  img.src = src;

  const closeBtn = document.createElement("button");
  closeBtn.className = "lightbox-close";
  closeBtn.textContent = "\u00d7";

  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") closeLightbox();
  };

  const closeLightbox = () => {
    overlay.remove();
    document.removeEventListener("keydown", onKey);
  };

  closeBtn.addEventListener("click", closeLightbox);
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) closeLightbox();
  });
  document.addEventListener("keydown", onKey);

  overlay.appendChild(img);
  overlay.appendChild(closeBtn);
  document.body.appendChild(overlay);

  requestAnimationFrame(() => overlay.classList.add("lightbox-visible"));
}
