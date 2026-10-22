export interface ShellState {
    top: number;
    left: number;
    width: number;
    height: number;
    keyboard: boolean;
    scale: number;
}
export interface ShellOptions {
    regions?: {
        header: HTMLElement;
        content: HTMLElement;
        bottom: HTMLElement;
        flow: HTMLElement;
    };
    followEnd?: boolean;
    initialScroll?: "start" | "end";
    endEditingOnHidden?: boolean;
}
export interface KeyboardHint {
    /** Keyboard end frame overlapping the web view, CSS px; 0 when hiding. */
    overlap: number;
    /** Animation duration in milliseconds, 0–1000. */
    duration: number;
    /** CSS timing function matching the native animation; defaults to `ease`. */
    easing?: string;
}
export interface ShellHandle {
    refresh(): void;
    scrollToEnd(): void;
    keyboardHint(hint: KeyboardHint): void;
    getState(): Readonly<ShellState>;
    destroy(): void;
}
export declare function attachShell(root: HTMLElement, options?: ShellOptions): ShellHandle;
export declare function attachTextarea(element: HTMLTextAreaElement): () => void;
