import { expect, test } from "bun:test";
import { createIngestor } from "../src/ingest";
import { classifyKind, createJevGate, DEFAULT_JEV_MODEL, type DecisionsFn } from "../src/jev";
import { createMemorySearchTool } from "../src/tools";

const noTimer = () => () => {};

test("user classification requires confidence and uses request instead of note", async () => {
    const questions: unknown[] = [];
    let choice = "decision";
    let confidence: number | undefined = 0.6;
    const decide: DecisionsFn = async (input) => {
        questions.push(input.questions[0]);
        return { answers: [{ type: "choice", name: "kind", choice, confidence }] };
    };
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBe("decision");
    expect(questions[0]).toEqual({
        type: "choice",
        name: "kind",
        instructions: "Classify this user message from a coding session.",
        choices: [
            { value: "decision", description: "A design decision" },
            { value: "constraint", description: "A requirement or constraint" },
            { value: "request", description: "Other user requests" },
        ],
    });
    choice = "constraint";
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBe("constraint");
    choice = "task";
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBe("task");
    choice = "constraint";
    confidence = 0.599;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    confidence = undefined;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    confidence = Number.NaN;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    confidence = 1;
    choice = "request";
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    choice = "decision";
    confidence = undefined;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBe("decision");
});

test("failure starts cooldown, expiry retries, and success permits a new cooldown", async () => {
    let now = 0;
    let calls = 0;
    const logs: string[] = [];
    const decide = createJevGate({
        cooldownMs: 100,
        now: () => now,
        scheduleTimeout: noTimer,
        log: (message) => logs.push(message),
        async decide() {
            calls++;
            if (calls === 1) {
                throw new Error("Insufficient balance token=secret-value");
            }
            if (calls === 3) {
                return { answers: [{ type: "error", name: "kind", reason: "offline" }] };
            }
            return { answers: [{ type: "choice", name: "kind", choice: "decision" }] };
        },
    });
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "a")).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "b")).toBeNull();
    expect(calls).toBe(1);
    expect(logs).toEqual(["Jev cooldown started: Insufficient balance token=[redacted]"]);
    now = 100;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "c")).toBe("decision");
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "d")).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "e")).toBeNull();
    expect(calls).toBe(3);
    expect(logs).toEqual(["Jev cooldown started: Insufficient balance token=[redacted]", "Jev cooldown started: offline"]);
});

test("timeout starts cooldown without real timers", async () => {
    let calls = 0;
    let expire: (() => void) | undefined;
    const logs: string[] = [];
    const decide = createJevGate({
        cooldownMs: 100,
        now: () => 0,
        log: (message) => logs.push(message),
        scheduleTimeout(callback) { expire = callback; return () => {}; },
        decide() { calls++; return new Promise(() => {}); },
    });
    const first = classifyKind(decide, DEFAULT_JEV_MODEL, "a");
    expire!();
    expect(await first).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "b")).toBeNull();
    expect(calls).toBe(1);
    expect(logs).toEqual(["Jev cooldown started: Jev request timed out"]);
});

test("late success cannot clear a concurrent failure cooldown", async () => {
    let calls = 0;
    let succeed: ((value: Awaited<ReturnType<DecisionsFn>>) => void) | undefined;
    const decide = createJevGate({
        cooldownMs: 100,
        now: () => 0,
        scheduleTimeout: noTimer,
        log() {},
        decide() {
            calls++;
            if (calls === 1) {
                return new Promise((resolve) => { succeed = resolve; });
            }
            throw new Error("offline");
        },
    });
    const first = classifyKind(decide, DEFAULT_JEV_MODEL, "a");
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "b")).toBeNull();
    succeed!({ answers: [{ type: "choice", name: "kind", choice: "task" }] });
    expect(await first).toBe("task");
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "c")).toBeNull();
    expect(calls).toBe(2);
});

test("ingest and search share the cooldown", async () => {
    let now = 0;
    let calls = 0;
    const decide = createJevGate({
        cooldownMs: 100,
        now: () => now,
        scheduleTimeout: noTimer,
        log() {},
        async decide(input) {
            calls++;
            if (calls === 1) { throw new Error("offline"); }
            return { answers: [{ type: "predicate", name: input.questions[0].name, probability: 0.9 }] };
        },
    });
    const ingestor = createIngestor({
        client: {
            async claimContinuation() { return { claimed: false }; },
            async putSession() {},
            async ingest() { return { segments: 1, skipped: 0 }; },
        },
        storage: { async get() { return undefined; }, async set() {} },
        async loadContext() {
            return [{ id: "a", type: "assistant", time: { created: 0 }, content: [{ type: "text", text: "note" }] }];
        },
        async putInstruction() {},
        decide,
        jevModel: DEFAULT_JEV_MODEL,
        harness: "h",
        project: "p",
        log() {},
    });
    const tool = createMemorySearchTool({
        client: { async search() { return { hits: [{ id: "1", score: 1, kind: "task", text: "memory", session: "s", ts: "t" }] }; } },
        decide,
        jevModel: DEFAULT_JEV_MODEL,
        harness: "h",
        project: "p",
    });
    const context = { sessionID: "s", signal: new AbortController().signal };
    await ingestor.flush("s");
    expect((await tool.execute({ query: "q" }, context)).content).toContain("memory");
    expect(calls).toBe(1);
    now = 100;
    expect((await tool.execute({ query: "q" }, context)).content).toContain("memory");
    expect(calls).toBe(2);
});
