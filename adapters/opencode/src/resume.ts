import { recordActivity, type ActivitySink } from "./activity-log";
import { partSize } from "./switch";

type ResumeOptions = { idleMinutes: number; minContextTokens: number };
type ResumeState = { lastRequestAt: number };

export function shouldCompactOnResume(input: {
    now: number;
    lastRequestAt?: number;
    idleMinutes: number;
    estimatedTokens: number;
    minContextTokens: number;
}): boolean {
    return typeof input.lastRequestAt === "number" && Number.isFinite(input.lastRequestAt)
        && input.now - input.lastRequestAt > input.idleMinutes * 60000
        && input.estimatedTokens >= input.minContextTokens;
}

function estimateContextTokens(messages: unknown[]): number {
    let size = 0;
    for (const value of messages) {
        if (!value || typeof value !== "object") {
            continue;
        }
        const message = value as Record<string, unknown>;
        if (typeof message.text === "string") {
            size += partSize({ type: "text", text: message.text });
        }
        if (!Array.isArray(message.content)) {
            continue;
        }
        for (const value of message.content) {
            if (value && typeof value === "object" && "type" in value && value.type === "tool" && "state" in value
                && value.state && typeof value.state === "object") {
                const state = value.state as Record<string, unknown>;
                size += partSize({ type: "tool-call", input: state.input });
                if (Array.isArray(state.content)) {
                    size += partSize({ type: "tool-result", result: { type: "content", value: state.content } });
                }
                if (state.error !== undefined) {
                    size += partSize({ type: "tool-result", result: { type: "json", value: state.error } });
                }
            } else {
                size += partSize(value);
            }
        }
    }
    return size / 4;
}

export function createResumeController(deps: {
    options?: ResumeOptions;
    storage: { get(key: string): Promise<unknown>; set(key: string, value: ResumeState): Promise<unknown> };
    loadContext(sessionID: string): Promise<unknown[]>;
    compact(input: { sessionID: string; delivery: "steer" }): Promise<unknown>;
    now(): number;
    project?: string;
    activity?: ActivitySink;
    log(message: string, error?: unknown): void;
}) {
    const states = new Map<string, ResumeState>();
    const chains = new Map<string, Promise<void>>();

    function serialize(sessionID: string, action: () => Promise<void>): Promise<void> {
        const current = (chains.get(sessionID) ?? Promise.resolve()).then(action).catch((error: unknown) => {
            deps.log(`Failed to process resume for session ${sessionID}`, error);
        });
        chains.set(sessionID, current);
        void current.then(() => {
            if (chains.get(sessionID) === current) {
                chains.delete(sessionID);
            }
        });
        return current;
    }

    async function persist(sessionID: string, lastRequestAt: number): Promise<void> {
        const state = { lastRequestAt };
        states.set(sessionID, state);
        await deps.storage.set(`memento/resume/${sessionID}`, state);
    }

    function onContext(sessionID: string): Promise<void> {
        return serialize(sessionID, () => persist(sessionID, deps.now()));
    }

    function onPrompt(sessionID: string): Promise<void> {
        if (!deps.options) {
            return Promise.resolve();
        }
        const options = deps.options;
        return serialize(sessionID, async () => {
            const stored = states.get(sessionID) ?? await deps.storage.get(`memento/resume/${sessionID}`);
            const lastRequestAt = stored && typeof stored === "object" && "lastRequestAt" in stored
                && typeof stored.lastRequestAt === "number" && Number.isFinite(stored.lastRequestAt)
                ? stored.lastRequestAt : undefined;
            const now = deps.now();
            if (!shouldCompactOnResume({ now, lastRequestAt, ...options, estimatedTokens: options.minContextTokens })) {
                return;
            }
            const estimatedTokens = estimateContextTokens(await deps.loadContext(sessionID));
            if (!shouldCompactOnResume({ now, lastRequestAt, ...options, estimatedTokens })) {
                return;
            }
            deps.log(`resume compaction for ${sessionID} after ${Math.floor((now - lastRequestAt!) / 60000)} min idle, ~${Math.round(estimatedTokens)} tokens`);
            await deps.compact({ sessionID, delivery: "steer" });
            recordActivity(deps.activity, { level: "info", event: "resume", session: sessionID, project: deps.project, detail: { idleMinutes: (now - lastRequestAt!) / 60000, estimatedTokens } });
            await persist(sessionID, now);
        });
    }

    async function forget(sessionID: string): Promise<void> {
        while (chains.has(sessionID)) {
            await chains.get(sessionID);
        }
        states.delete(sessionID);
    }

    return { onContext, onPrompt, forget };
}
