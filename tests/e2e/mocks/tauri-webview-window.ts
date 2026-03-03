const mockWindow = {
  label: 'main',
  listen: async (_event: string, _handler: Function) => () => {},
  emit: async (_event: string, _payload?: any) => {},
  onCloseRequested: async (_handler: Function) => () => {},
};

export function getCurrentWebviewWindow() {
  return mockWindow;
}

export class WebviewWindow {
  label: string;
  constructor(label: string) { this.label = label; }
}
