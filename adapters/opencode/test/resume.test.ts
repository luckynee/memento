import { expect, test } from "bun:test";
import type { ActivityEntry } from "../src/activity-log";
import { createResumeController, shouldCompactOnResume } from "../src/resume";

test("resume decision requires a previous request, strictly more idle time, and enough context", () => {
    const input = { now: 120000, idleMinutes: 1, minContextTokens: 100, estimatedTokens: 100 };
    expect(shouldCompactOnResume(input)).toBe(false);
    expect(shouldCompactOnResume({ ...input, lastRequestAt: NaN })).toBe(false);
    expect(shouldCompactOnResume({ ...input, lastRequestAt: 60000 })).toBe(false);
    expect(shouldCompactOnResume({ ...input, lastRequestAt: 59999, estimatedTokens: 99 })).toBe(false);
    expect(shouldCompactOnResume({ ...input, lastRequestAt: 59999 })).toBe(true);
    expect(shouldCompactOnResume({ ...input, lastRequestAt: 0 })).toBe(true);
});

test("resume controller persists context activity and compacts once on a qualifying prompt", async () => {
    const values = new Map<string, unknown>();
    const calls: unknown[] = [];
    const logs: string[] = [];
    const activity: ActivityEntry[] = [];
    let now = 1000;
    let loads = 0;
    const controller = createResumeController({
        options: { idleMinutes: 0.5, minContextTokens: 10 },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) { values.set(key, value); },
        },
        async loadContext() {
            loads++;
            return [
                { type: "user", text: "1234567890123456" },
                { type: "assistant", content: [{ type: "text", text: "12345678" }, {
                    type: "tool", state: { input: "1234", content: [{ type: "text", text: "1234567890" }] },
                }] },
            ];
        },
        async compact(input) { calls.push(input); },
        now: () => now,
        project: "p",
        activity: { record(entry) { activity.push(entry); } },
        log: (message) => logs.push(message),
    });
    await controller.onPrompt("new");
    expect(loads).toBe(0);
    await controller.onContext("s");
    expect(values.get("memento/resume/s")).toEqual({ lastRequestAt: 1000 });
    now = 31000;
    await controller.onPrompt("s");
    expect(loads).toBe(0);
    now++;
    await Promise.all([controller.onPrompt("s"), controller.onPrompt("s")]);
    expect(calls).toEqual([{ sessionID: "s", delivery: "steer" }]);
    expect(values.get("memento/resume/s")).toEqual({ lastRequestAt: 31001 });
    expect(logs).toEqual(["resume compaction for s after 0 min idle, ~10 tokens"]);
    expect(activity).toEqual([{ level: "info", event: "resume", session: "s", project: "p", detail: { idleMinutes: 30001 / 60000, estimatedTokens: 10 } }]);
    await controller.forget("s");
    now += 30001;
    await controller.onPrompt("s");
    expect(calls).toHaveLength(2);
});

test("resume controller skips small contexts and disabled prompts", async () => {
    const values = new Map<string, unknown>([["memento/resume/s", { lastRequestAt: 0 }]]);
    let calls = 0;
    let loads = 0;
    const deps = {
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
        },
        async loadContext() { loads++; return [{ type: "user", text: "small" }]; },
        async compact() { calls++; },
        now: () => 100000,
        log() {},
    };
    await createResumeController(deps).onPrompt("s");
    expect(loads).toBe(0);
    await createResumeController({ ...deps, options: { idleMinutes: 1, minContextTokens: 10 } }).onPrompt("s");
    expect(loads).toBe(1);
    expect(calls).toBe(0);
    expect(values.get("memento/resume/s")).toEqual({ lastRequestAt: 0 });
});

test("resume controller logs and swallows storage, context, and compaction errors", async () => {
    for (const failing of ["get", "set", "load", "compact"]) {
        const errors: unknown[] = [];
        const controller = createResumeController({
            options: { idleMinutes: 1, minContextTokens: 1 },
            storage: {
                async get() { if (failing === "get") { throw new Error(failing); } return { lastRequestAt: 0 }; },
                async set() { if (failing === "set") { throw new Error(failing); } },
            },
            async loadContext() { if (failing === "load") { throw new Error(failing); } return [{ type: "user", text: "1234" }]; },
            async compact() { if (failing === "compact") { throw new Error(failing); } },
            now: () => 100000,
            log: (_message, error) => { if (error) { errors.push(error); } },
        });
        if (failing === "set") {
            await controller.onContext("s");
        } else {
            await controller.onPrompt("s");
        }
        expect(errors).toHaveLength(1);
        expect(String(errors[0])).toContain(failing);
    }
});
