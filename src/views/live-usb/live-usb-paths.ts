import { getPlatform } from "../../utils/platform.js";

/** The ISO path as shown before saving; the backend reports the real one afterwards. */
export function isoTargetPath(folder: string, name: string): string {
  if (!folder) return name;
  const separator = getPlatform() === "windows" ? "\\" : "/";
  // A root such as `/` or `C:\` trims to "" or "C:", so adding one separator back is right.
  return `${folder.replace(/[\\/]+$/, "")}${separator}${name}`;
}
