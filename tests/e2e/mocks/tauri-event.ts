const listeners: Record<string, Array<(event: any) => void>> = {};

export async function listen(eventName: string, callback: (event: any) => void): Promise<() => void> {
  if (!listeners[eventName]) {
    listeners[eventName] = [];
  }
  listeners[eventName].push(callback);

  // Store on window for test access
  (window as any).__test_listeners = (window as any).__test_listeners || {};
  (window as any).__test_listeners[eventName] = listeners[eventName];

  return () => {
    const idx = listeners[eventName].indexOf(callback);
    if (idx >= 0) listeners[eventName].splice(idx, 1);
  };
}

export async function emit(eventName: string, payload?: any): Promise<void> {
  const cbs = listeners[eventName] || [];
  for (const cb of cbs) {
    cb({ event: eventName, id: 0, payload });
  }
}
