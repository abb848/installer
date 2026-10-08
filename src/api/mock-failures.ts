/** One-shot failures for browser development and E2E tests, never native IPC. */
export function failMockOperation(operation: "flash" | "proxmox" | "utm") {
  if (!import.meta.env.DEV || "__TAURI__" in window) return;

  const key = "hai:mock-failure";
  const scenario = sessionStorage.getItem(key);
  const failures: Record<string, { operation: string; message: string }> = {
    "flash-write": { operation: "flash", message: "Write failed: I/O error" },
    "flash-disconnected": {
      operation: "flash",
      message: "Drive disconnected",
    },
    "proxmox-install": {
      operation: "proxmox",
      message: "Proxmox API error: storage unavailable",
    },
    "utm-create": {
      operation: "utm",
      message: "UTM error: Automation permission denied",
    },
  };
  const failure = scenario ? failures[scenario] : undefined;
  if (failure?.operation !== operation) return;

  sessionStorage.removeItem(key);
  // Tauri serializes command errors as strings, not JavaScript Error objects.
  throw failure.message;
}
