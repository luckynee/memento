import { appendFile, mkdir, rename, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export type ActivityEvent = "register" | "ingest" | "compaction" | "switch" | "subagent_digest" | "resume" | "continuation" | "error";
export type ActivityLevel = "info" | "warn" | "error";
export type ActivityDetail = Record<string, number | string | boolean>;
export type ActivityEntry = {
    level: ActivityLevel;
    event: ActivityEvent;
    session?: string;
    project?: string;
    message?: string;
    detail?: ActivityDetail;
};
export type ActivitySink = { record(entry: ActivityEntry): void };

export function recordActivity(activity: ActivitySink | undefined, entry: ActivityEntry): void {
    try {
        activity?.record(entry);
    } catch {
        return;
    }
}
export type ActivityLogOptions = {
    path?: string;
    maxBytes?: number;
    now?: () => number;
};

const detailFields = new Set([
    "count", "items", "segments", "skipped", "bytes", "chars", "durationMs",
    "idleMinutes", "estimatedTokens", "model", "fromModel", "toModel", "mode",
    "source", "reason", "outcome", "errorClass", "success", "claimed", "reused",
]);
const stringDetailFields = new Set(["model", "fromModel", "toModel", "mode", "source", "reason", "outcome", "errorClass"]);
const safeIdentifier = /^[\w./:@+-]{1,200}$/;

function safeString(value: string): string | undefined {
    return safeIdentifier.test(value) && !value.includes("://") && !value.includes("@") ? value : undefined;
}

function safeMessage(message: string): string {
    const trimmed = message.replace(/[\r\n\t]+/g, " ");
    if (/\b(?:bearer|token|authorization|api[_-]?key|password|secret)\b|[a-z][a-z\d+.-]*:\/\/|[{}[\]"'`]/i.test(trimmed)) {
        return "[redacted]";
    }
    return Array.from(trimmed).slice(0, 300).join("");
}

function sanitizeDetail(detail: ActivityDetail | undefined): ActivityDetail | undefined {
    if (!detail) {
        return undefined;
    }
    const safe: ActivityDetail = {};
    for (const [key, value] of Object.entries(detail)) {
        if (!detailFields.has(key)) {
            continue;
        }
        if (typeof value === "string") {
            const text = stringDetailFields.has(key) ? safeString(value) : undefined;
            if (text !== undefined) {
                safe[key] = text;
            }
        } else if (typeof value === "boolean" || (typeof value === "number" && Number.isFinite(value))) {
            safe[key] = value;
        }
    }
    return Object.keys(safe).length ? safe : undefined;
}

export function createActivityLog(options: ActivityLogOptions = {}): {
    record(entry: ActivityEntry): void;
    flush(): Promise<void>;
} {
    const path = options.path ?? join(process.env.XDG_STATE_HOME || join(homedir(), ".local", "state"), "memento", "opencode-activity.jsonl");
    const maxBytes = options.maxBytes ?? 1_000_000;
    const now = options.now ?? Date.now;
    let pending = Promise.resolve();

    return {
        record(entry) {
            try {
                const line = JSON.stringify({
                    ts: now(),
                    level: entry.level,
                    event: entry.event,
                    session: entry.session === undefined ? undefined : safeString(entry.session),
                    project: entry.project === undefined ? undefined : safeString(entry.project),
                    message: entry.message === undefined ? undefined : safeMessage(entry.message),
                    detail: sanitizeDetail(entry.detail),
                }) + "\n";
                pending = pending.then(async () => {
                    await mkdir(dirname(path), { recursive: true, mode: 0o700 });
                    const size = await stat(path).then((file) => file.size, (error: NodeJS.ErrnoException) => {
                        if (error.code === "ENOENT") {
                            return 0;
                        }
                        throw error;
                    });
                    if (size > maxBytes) {
                        await rename(path, `${path}.1`);
                    }
                    await appendFile(path, line, { mode: 0o600 });
                }).catch(() => {});
            } catch {
                return;
            }
        },
        flush() {
            return pending;
        },
    };
}
