export async function open(_options?: any): Promise<string | string[] | null> {
  const override = (window as any).__mockDialogPath;
  if (override) return override;
  return '/mock/workspace';
}

export async function save(_options?: any): Promise<string | null> {
  return null;
}

export async function message(_msg: string, _options?: any): Promise<void> {}
export async function ask(_msg: string, _options?: any): Promise<boolean> { return false; }
export async function confirm(_msg: string, _options?: any): Promise<boolean> {
  const override = (window as any).__mockDialogConfirm;
  if (typeof override === 'boolean') return override;
  return false;
}
