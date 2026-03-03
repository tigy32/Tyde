export interface RemoteWorkspaceTarget {
  host: string;
  path: string;
}

export function parseRemoteWorkspaceUri(
  uri: string,
): RemoteWorkspaceTarget | null {
  const trimmed = uri.trim();
  if (!trimmed.startsWith("ssh://")) return null;
  const rest = trimmed.slice("ssh://".length);
  const slashIdx = rest.indexOf("/");
  if (slashIdx <= 0) return null;
  const host = rest.slice(0, slashIdx).trim();
  const path = rest.slice(slashIdx).trim();
  if (!host || !path) return null;
  return { host, path: path.startsWith("/") ? path : `/${path}` };
}

export function normalizeRemoteWorkspaceInput(raw: string): string | null {
  const input = raw.trim();
  if (!input) return null;

  const parsedUri = parseRemoteWorkspaceUri(input);
  if (parsedUri) {
    return `ssh://${parsedUri.host}${parsedUri.path}`;
  }

  const colonIdx = input.indexOf(":");
  if (colonIdx <= 0 || colonIdx === input.length - 1) return null;

  const host = input.slice(0, colonIdx).trim();
  const rawPath = input.slice(colonIdx + 1).trim();
  if (!host || !rawPath) return null;
  const path = rawPath.startsWith("/") ? rawPath : `/${rawPath}`;
  return `ssh://${host}${path}`;
}

export function workspaceDisplayName(workspacePath: string): string {
  const remote = parseRemoteWorkspaceUri(workspacePath);
  if (!remote) {
    return (
      workspacePath.split("/").pop() ||
      workspacePath.split("\\").pop() ||
      workspacePath
    );
  }

  const base = remote.path.split("/").filter(Boolean).pop() || "/";
  return `${base} @ ${remote.host}`;
}
