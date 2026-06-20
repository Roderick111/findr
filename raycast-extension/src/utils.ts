import { environment, getPreferenceValues } from "@raycast/api";
import type { SearchResponse } from "./types";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  createWriteStream,
  readFileSync,
  renameSync,
  unlinkSync,
  openSync,
  closeSync,
} from "fs";
import { createHash } from "crypto";
import { execFile } from "child_process";
import { join } from "path";
import { get } from "https";

const GITHUB_REPO = "Roderick111/findr";

/** Platform-specific binary names from GitHub Releases. */
const FINDR_BINARY =
  process.platform === "win32"
    ? "findr-windows-x86_64.exe"
    : process.platform === "linux"
      ? "findr-linux-x86_64"
      : "findr-macos-universal";
const FINDR_OCR_BINARY = "findr-ocr-macos-universal"; // macOS only

/** Fallback SHA-256 when release ships without checksums.txt (fail closed otherwise). */
const EMBEDDED_CHECKSUMS: Record<string, Record<string, string>> = {
  "v1.4.5": {
    "findr-macos-universal":
      "9c0111e3aebd726b46fea449e2673c692b92c90adc3a69a988d3d61e16e84d1d",
    "findr-ocr-macos-universal":
      "f607eae1dc54d9e8956b00481dcd2c6f79f3883381c259849044877fffabfa47",
  },
};

/** Directory for downloaded binaries (persists across extension updates). */
function binDir(): string {
  const dir = join(environment.supportPath, "bin");
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
  }
  return dir;
}

/** Download a file from a URL, following redirects. Downloads to a temp file
 *  first, then renames on success. On failure the temp file is deleted so
 *  a broken download never blocks future retries. */
function downloadFile(url: string, dest: string): Promise<void> {
  const tmp = dest + ".tmp";
  return new Promise((resolve, reject) => {
    const file = createWriteStream(tmp);
    const fail = (err: Error) => {
      file.close();
      try {
        unlinkSync(tmp);
      } catch {
        /* already gone */
      }
      reject(err);
    };
    file.on("error", fail);
    const request = (u: string) => {
      get(u, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          const location = res.headers.location;
          res.resume(); // drain redirect response to release socket
          if (location) {
            request(location);
            return;
          }
        }
        if (res.statusCode !== 200) {
          fail(new Error(`Download failed: HTTP ${res.statusCode}`));
          return;
        }
        res.pipe(file);
        file.on("finish", () => {
          file.close();
          try {
            renameSync(tmp, dest);
            resolve();
          } catch (err) {
            fail(err as Error);
          }
        });
      }).on("error", fail);
    };
    request(url);
  });
}

/** Compute SHA-256 hex digest of a file. */
function sha256File(filePath: string): string {
  const data = readFileSync(filePath);
  return createHash("sha256").update(data).digest("hex");
}

/** Fetch text content from a URL, following redirects. Returns null on any error. */
function fetchText(url: string): Promise<string | null> {
  return new Promise((resolve) => {
    const request = (u: string) => {
      get(u, (res) => {
        if (res.statusCode === 302 || res.statusCode === 301) {
          const location = res.headers.location;
          res.resume();
          if (location) {
            request(location);
            return;
          }
        }
        if (res.statusCode !== 200) {
          res.resume();
          resolve(null);
          return;
        }
        let data = "";
        res.on("data", (chunk: string) => (data += chunk));
        res.on("end", () => resolve(data));
      }).on("error", () => resolve(null));
    };
    request(url);
  });
}

/** Parse checksums.txt (format: "sha256  filename" per line) into a map. */
function parseChecksums(content: string): Map<string, string> {
  const map = new Map<string, string>();
  for (const line of content.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    // Format: hash followed by two spaces and filename
    const match = trimmed.match(/^([a-f0-9]{64})\s+(.+)$/);
    if (match) {
      map.set(match[2], match[1]);
    }
  }
  return map;
}

/** Resolve expected SHA-256 from release checksums.txt or embedded per-version map. */
function resolveExpectedChecksum(
  filename: string,
  releaseTag: string,
  checksums: Map<string, string> | null,
): string | null {
  const fromFile = checksums?.get(filename);
  if (fromFile) return fromFile;
  return EMBEDDED_CHECKSUMS[releaseTag]?.[filename] ?? null;
}

/** Verify a downloaded file against expected checksum. Deletes file on mismatch. */
function verifyChecksumRequired(
  filePath: string,
  filename: string,
  expected: string | null,
): void {
  if (!expected) {
    try {
      unlinkSync(filePath);
    } catch {
      /* best effort */
    }
    throw new Error(
      `No checksum available for ${filename} — refusing to use unverified binary`,
    );
  }
  const actual = sha256File(filePath);
  if (actual !== expected) {
    try {
      unlinkSync(filePath);
    } catch {
      /* best effort */
    }
    throw new Error(
      `Checksum mismatch for ${filename}: expected ${expected}, got ${actual}`,
    );
  }
  console.log(`Checksum verified for ${filename}`);
}

let downloadInFlight: Promise<string> | null = null;

/** Acquire an exclusive lock file so parallel extension instances don't race downloads. */
async function withDownloadLock<T>(
  lockPath: string,
  fn: () => Promise<T>,
): Promise<T> {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    try {
      const fd = openSync(lockPath, "wx");
      closeSync(fd);
      try {
        return await fn();
      } finally {
        try {
          unlinkSync(lockPath);
        } catch {
          /* already released */
        }
      }
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
  }
  throw new Error("Timed out waiting for binary download lock");
}

/** Download findr binaries from the latest GitHub Release. */
export async function ensureFindrBinaries(): Promise<string> {
  const dir = binDir();
  const exe = process.platform === "win32" ? "findr.exe" : "findr";
  const findrPath = join(dir, exe);
  const ocrPath = join(dir, "findr-ocr");

  if (existsSync(findrPath)) {
    return findrPath;
  }

  if (downloadInFlight) {
    return downloadInFlight;
  }

  downloadInFlight = withDownloadLock(join(dir, ".download.lock"), async () => {
    if (existsSync(findrPath)) {
      return findrPath;
    }

    // Fetch latest release download URLs from GitHub API
    const releaseUrl = `https://api.github.com/repos/${GITHUB_REPO}/releases/latest`;
    const release: {
      tag_name: string;
      assets: { name: string; browser_download_url: string }[];
    } = await new Promise((resolve, reject) => {
      get(
        releaseUrl,
        {
          headers: {
            "User-Agent": "findr-raycast",
            Accept: "application/json",
          },
        },
        (res) => {
          let data = "";
          res.on("data", (chunk: string) => (data += chunk));
          res.on("end", () => {
            if (res.statusCode !== 200) {
              reject(
                new Error(
                  `GitHub API error: HTTP ${res.statusCode}${res.statusCode === 403 ? " (rate limit — try again later)" : ""}`,
                ),
              );
              return;
            }
            try {
              resolve(JSON.parse(data));
            } catch {
              reject(new Error("Failed to parse GitHub release"));
            }
          });
        },
      ).on("error", reject);
    });

    const findrAsset = release.assets?.find((a) => a.name === FINDR_BINARY);
    const ocrAsset = release.assets?.find((a) => a.name === FINDR_OCR_BINARY);
    const checksumAsset = release.assets?.find(
      (a) => a.name === "checksums.txt",
    );

    if (!findrAsset) {
      throw new Error("findr binary not found in latest GitHub release");
    }

    const releaseTag = release.tag_name ?? "unknown";

    // Prefer checksums.txt; fall back to embedded per-version hashes.
    let checksums: Map<string, string> | null = null;
    if (checksumAsset) {
      const content = await fetchText(checksumAsset.browser_download_url);
      if (content) {
        checksums = parseChecksums(content);
      }
    }

    const findrExpected = resolveExpectedChecksum(
      FINDR_BINARY,
      releaseTag,
      checksums,
    );

    await downloadFile(findrAsset.browser_download_url, findrPath);
    verifyChecksumRequired(findrPath, FINDR_BINARY, findrExpected);
    chmodSync(findrPath, 0o755);

    // findr-ocr is macOS-only (Swift). Linux/Windows use ocrs built into the Rust binary.
    if (ocrAsset && process.platform === "darwin") {
      const ocrExpected = resolveExpectedChecksum(
        FINDR_OCR_BINARY,
        releaseTag,
        checksums,
      );
      await downloadFile(ocrAsset.browser_download_url, ocrPath);
      verifyChecksumRequired(ocrPath, FINDR_OCR_BINARY, ocrExpected);
      chmodSync(ocrPath, 0o755);
    }

    return findrPath;
  });

  try {
    return await downloadInFlight;
  } finally {
    downloadInFlight = null;
  }
}

let chmodApplied = false;

export function getFindrPath(): string {
  const { findrPath } = getPreferenceValues<ExtensionPreferences>();

  // User override takes priority
  if (findrPath && existsSync(findrPath)) {
    return findrPath;
  }

  // Downloaded binary (from GitHub Releases)
  const exeName = process.platform === "win32" ? "findr.exe" : "findr";
  const downloaded = join(binDir(), exeName);
  if (existsSync(downloaded)) {
    if (!chmodApplied) {
      try {
        chmodSync(downloaded, 0o755);
      } catch {
        // May already be executable
      }
      chmodApplied = true;
    }
    return downloaded;
  }

  // Fallback: bundled binary (for local development)
  const bundled = join(environment.assetsPath, exeName);
  if (existsSync(bundled)) {
    if (!chmodApplied) {
      try {
        chmodSync(bundled, 0o755);
      } catch {
        // May already be executable
      }
      chmodApplied = true;
    }
    return bundled;
  }

  return downloaded; // Will trigger "binary not found" in search.tsx
}

export function getMaxResults(): number {
  const { maxResults } = getPreferenceValues<ExtensionPreferences>();
  const parsed = parseInt(maxResults, 10);
  return parsed > 0 ? parsed : 30;
}

export function getOpenRouterApiKey(): string {
  const { openrouterApiKey } = getPreferenceValues<ExtensionPreferences>();
  return openrouterApiKey?.trim() || "";
}

export function getScanScope(): string {
  const { scanScope } = getPreferenceValues<ExtensionPreferences>();
  return scanScope || "personal";
}

export function getCustomPaths(): string {
  const { customPaths } = getPreferenceValues<ExtensionPreferences>();
  return customPaths?.trim() || "";
}

export function getScanArgs(): string[] {
  const scope = getScanScope();
  const custom = getCustomPaths();
  const args = ["--preset", scope];
  if (custom) {
    args.push("--paths", custom);
  }
  return args;
}

export function getFindrEnv(): Record<string, string> {
  const env: Record<string, string> = {};
  const key = getOpenRouterApiKey();
  if (key) {
    env.OPENROUTER_API_KEY = key;
  }
  env.FINDR_SCAN_PRESET = getScanScope();
  const custom = getCustomPaths();
  if (custom) {
    env.FINDR_SCAN_PATHS = custom;
  }
  return env;
}

export function formatFileSize(bytes: number | null): string {
  if (bytes === null || bytes === undefined) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

export function formatRelativeDate(isoDate: string): string {
  if (!isoDate) return "";
  const date = new Date(isoDate);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));
  const time = date.toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
  });

  if (diffDays === 0) return `Today at ${time}`;
  if (diffDays === 1) return `Yesterday at ${time}`;
  const day = date.getDate();
  const month = date.toLocaleDateString("en-US", { month: "long" });
  const year = date.getFullYear();
  return `${day} ${month}, ${year} at ${time}`;
}

const FILE_TYPE_ICONS: Record<string, string> = {
  pdf: "📄",
  doc: "📝",
  docx: "📝",
  xls: "📊",
  xlsx: "📊",
  ppt: "📊",
  pptx: "📊",
  png: "🖼️",
  jpg: "🖼️",
  jpeg: "🖼️",
  gif: "🖼️",
  svg: "🖼️",
  webp: "🖼️",
  mp3: "🎵",
  mp4: "🎬",
  mov: "🎬",
  zip: "📦",
  tar: "📦",
  gz: "📦",
  md: "📋",
  txt: "📋",
  csv: "📋",
  json: "⚙️",
  yml: "⚙️",
  yaml: "⚙️",
  toml: "⚙️",
  rs: "🦀",
  ts: "💠",
  tsx: "💠",
  js: "💛",
  jsx: "💛",
  py: "🐍",
  go: "🐹",
  html: "🌐",
  css: "🎨",
  sh: "⚡",
};

export function getFileIcon(ext: string | null): string {
  if (!ext) return "📁";
  return FILE_TYPE_ICONS[ext] || "📁";
}

/** Fire-and-forget interaction tracking for frequency-based ranking. */
export function trackInteraction(path: string, action: string): void {
  execFile(getFindrPath(), ["track", path, "--action", action], () => {});
}

/** Parse findr search stdout; surfaces JSON `error`/`hint` before generic failures. */
export function parseSearchStdout(stdout: string): SearchResponse {
  const trimmed = stdout.trim();
  if (!trimmed) {
    throw new Error("Empty search output from findr");
  }

  let data: SearchResponse;
  try {
    data = JSON.parse(trimmed) as SearchResponse;
  } catch {
    throw new Error("Failed to parse search output");
  }

  if (!Array.isArray(data.results)) {
    throw new Error("Invalid search response: missing results array");
  }

  if (data.mode === "error" || data.error) {
    throw new Error(data.error || data.hint || data.message || "Search failed");
  }

  return data;
}
