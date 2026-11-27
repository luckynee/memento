import { expect, test } from "bun:test";
import { createIngestor } from "../src/ingest";
import { classifyKind, createJevGate, DEFAULT_JEV_MODEL, stripScaffolding, type DecisionsFn } from "../src/jev";
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
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBeNull();
    confidence = undefined;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    confidence = Number.NaN;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBeNull();
    confidence = Number.POSITIVE_INFINITY;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBeNull();
    confidence = 0.6;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBe("constraint");
    confidence = 1;
    choice = "request";
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "user text", "user")).toBeNull();
    choice = "decision";
    confidence = undefined;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "assistant text")).toBe("decision");
});

test("scaffold regression preserves unclosed prose", () => {
    const text = "We noticed <system-reminder> and kept the important decision.";
    expect(stripScaffolding(text)).toBe(text);
});

test("scaffold regression preserves backticked mention", () => {
    const text = "Check how `<system-reminder>` works before changing this.";
    expect(stripScaffolding(text)).toBe(text);
});

test("scaffold regression handles interleaved names", () => {
    const text = "Header <system-reminder>mentions <subagent> tag</system-reminder> IMPORTANT DECISION";
    expect(stripScaffolding(text)).toBe("Header  IMPORTANT DECISION");
});

test("scaffold regression strips a complete block after an unclosed tag", () => {
    const text = "Before <system-reminder> keep <subagent>hidden</subagent> after";
    expect(stripScaffolding(text)).toBe("Before <system-reminder> keep  after");
});

test("strips the default Rust scaffolding tags and unwraps command arguments", () => {
    const tags = [
        "system-reminder", "skill_content", "environment_context", "user_instructions",
        "command-message", "command-name", "local-command-stdout", "local-command-stderr",
        "local-command-caveat", "subagent", "memento-digest", "memento-memory", "memento-continuation",
    ];
    for (const tag of tags) {
        expect(stripScaffolding(`before <${tag} key="a > b">\nsecret\n</${tag}> after`)).toBe("before  after");
    }
    const cases = [
        ["a<command-args x='1'>keep</command-args>b", "akeepb"],
        ["a<SYSTEM-REMINDER>hide</sYsTeM-rEmInDeR>b", "ab"],
        ["a<subagent>outer<subagent>inner</subagent>end</subagent>b", "ab"],
        ["a<subagent>x</subagent><skill_content>y</skill_content>b", "ab"],
        ["a<subagent>x<system-reminder>y</system-reminder>z</subagent>b", "ab"],
        ["a<system-reminder attr='x'>unfinished", "a<system-reminder attr='x'>unfinished"],
        ["a<system-reminderish>keep</system-reminderish>b", "a<system-reminderish>keep</system-reminderish>b"],
        ["é<COMMAND-ARGS>🙂</COMMAND-ARGS>界", "é🙂界"],
        ["a</subagent>b", "a</subagent>b"],
        [" a<subagent>unfinished", " a<subagent>unfinished"],
        [" a<subagent attr='unfinished>", " a<subagent attr='unfinished>"],
        ["  unchanged  ", "  unchanged  "],
    ];
    for (const [text, expected] of cases) {
        expect(stripScaffolding(text)).toBe(expected);
    }
});

test("classifies stripped text before truncation and skips empty scaffolding", async () => {
    const inputs: string[] = [];
    const decide: DecisionsFn = async (input) => {
        inputs.push(input.input[0].text);
        return { answers: [{ type: "choice", name: "kind", choice: "decision", confidence: 0.6 }] };
    };
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, `<system-reminder>${"x".repeat(5000)}</system-reminder>real detail`, "user")).toBe("decision");
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "<memento-continuation>hidden</memento-continuation>")).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, " \n ", "user")).toBeNull();
    expect(inputs).toEqual(["real detail"]);
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
    expect(logs).toEqual(["Jev cooldown started: Insufficient balance redacted=redacted"]);
    now = 100;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "c")).toBe("decision");
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "d")).toBeNull();
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "e")).toBeNull();
    expect(calls).toBe(3);
    expect(logs).toEqual(["Jev cooldown started: Insufficient balance redacted=redacted", "Jev cooldown started: offline"]);
});

test("provider rate limits extend cooldown and produce safe structured logs", async () => {
    let now = 0;
    let calls = 0;
    const logs: string[] = [];
    const decide = createJevGate({
        cooldownMs: 600_000,
        now: () => now,
        scheduleTimeout: noTimer,
        log: (message) => logs.push(message),
        async decide() {
            calls++;
            throw { _tag: "ProviderRequestError", reason: "rate_limit", status: 429, retryAfterMs: 48_535_000 };
        },
    });
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "a")).toBeNull();
    expect(logs).toEqual(["Jev cooldown started: rate_limit status=429 retry in 13h28m55s"]);
    now = 48_534_999;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "b")).toBeNull();
    expect(calls).toBe(1);
    now = 48_535_000;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "c")).toBeNull();
    expect(calls).toBe(2);
});

test("parses JSON error messages and caps provider retry at one day", async () => {
    let now = 0;
    let calls = 0;
    const logs: string[] = [];
    const decide = createJevGate({
        cooldownMs: 600_000,
        now: () => now,
        scheduleTimeout: noTimer,
        log: (message) => logs.push(message),
        async decide() {
            calls++;
            throw new Error(JSON.stringify({ _tag: "ProviderRequestError", reason: "rate_limit", status: 429, retryAfterMs: 100_000_000 }));
        },
    });
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "a")).toBeNull();
    expect(logs).toEqual(["Jev cooldown started: rate_limit status=429 retry in 24h"]);
    now = 86_399_999;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "b")).toBeNull();
    expect(calls).toBe(1);
    now = 86_400_000;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "c")).toBeNull();
    expect(calls).toBe(2);
});

test("short provider retry preserves configured cooldown and finds nested errors", async () => {
    let now = 0;
    let calls = 0;
    const logs: string[] = [];
    const decide = createJevGate({
        cooldownMs: 600_000,
        now: () => now,
        scheduleTimeout: noTimer,
        log: (message) => logs.push(message),
        async decide() {
            calls++;
            throw { cause: { data: { error: { _tag: "ProviderRequestError", status: 429, retryAfterMs: 1000 } } } };
        },
    });
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "a")).toBeNull();
    expect(logs).toEqual(["Jev cooldown started: ProviderRequestError status=429 retry in 10m"]);
    now = 599_999;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "b")).toBeNull();
    expect(calls).toBe(1);
    now = 600_000;
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "c")).toBeNull();
    expect(calls).toBe(2);
});

test("plain error fallback removes activity-log redaction triggers and credentials", async () => {
    const logs: string[] = [];
    const decide = createJevGate({
        cooldownMs: 100,
        now: () => 0,
        scheduleTimeout: noTimer,
        log: (message) => logs.push(message),
        async decide() {
            throw new Error(`Failed "request" at https://example.invalid/path token=private-value`);
        },
    });
    expect(await classifyKind(decide, DEFAULT_JEV_MODEL, "a")).toBeNull();
    expect(logs).toEqual(["Jev cooldown started: Failed request at redacted=redacted"]);
    expect(logs[0]).not.toMatch(/\b(?:bearer|token|authorization|api[_-]?key|password|secret)\b|[a-z][a-z\d+.-]*:\/\/|[{}[\]"'`]/i);
    expect(logs[0]).not.toContain("private-value");
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
