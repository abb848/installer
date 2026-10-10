import { getPlatform } from "../../utils/platform.js";

/** The ISO path as shown before saving; the backend reports the real one afterwards. */
export function isoTargetPath(folder: string, name: string): string {
  const separator = getPlatform() === "windows" ? "\\" : "/";
  const trimmed = folder.replace(/[\\/]+$/, "");
  return trimmed ? `${trimmed}${separator}${name}` : name;
}
