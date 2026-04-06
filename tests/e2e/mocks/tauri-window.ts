const mockWindow = {
  label: "main",
  async outerPosition() {
    return { x: 0, y: 0 };
  },
  async outerSize() {
    return { width: 1280, height: 720 };
  },
  async isMaximized() {
    return false;
  },
  async maximize() {},
  async setPosition(_pos: any) {},
  async setSize(_size: any) {},
  async listen(_event: string, _handler: Function) {
    return () => {};
  },
  async emit(_event: string, _payload?: any) {},
  async onCloseRequested(_handler: Function) {
    return () => {};
  },
};

export function getCurrentWindow() {
  return mockWindow;
}
