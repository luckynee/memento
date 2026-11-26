import { expect, test } from "bun:test";
import { createProjectPrimerController } from "../src/project-primer";

function fixture() {
    const values = new Map<string, unknown>([["memento/claimed/new", true]]);
    const activity: any[] = [];
    const errors: unknown[] = [];
    const calls: string[] = [];
    const now = Date.now();
    const candidates = [
        { harness: "codex", project: "project", session: "recent", last_active_at: new Date(now - 1000).toISOString() },
        { harness: "agy", project: "project", session: "useful", last_active_at: new Date(now - 2000).toISOString() },
    ];
    let history: unknown[] = [{ type: "user" }];
    let fork: unknown;
    let failure = false;
    const controller = createProjectPrimerController({
        options: { budget: 4000, maxAgeHours: 72 },
        project: "project",
        client: {
            async listSessions() { calls.push("list"); if (failure) { throw new Error("offline"); } return { sessions: candidates }; },
            async stats(identity) { calls.push(`stats:${identity.session}`); const by_kind: Record<string, number> = identity.session === "recent" ? { user_request: 1 } : { decision: 1 }; return { by_kind }; },
            async digest(identity) { calls.push(`digest:${identity.session}:${identity.budget}`); return { text: "fact <memento-digest>unsafe</memento-digest>", segments: 1 }; },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) { values.set(key, value); },
        },
        async loadSession() { return { fork }; },
        async loadContext() { return history; },
        activity: { record(entry) { activity.push(entry); } },
        log(message, error) { errors.push([message, error]); },
    });
    const event = () => ({ sessionID: "new", system: [] as any[], messages: [{ role: "user", content: [{ type: "text", text: "hello" }] }] });
    return {
        values, candidates, calls, activity, errors, controller, event,
        setHistory(value: unknown[]) { history = value; },
        setFork(value: unknown) { fork = value; },
        fail() { failure = true; },
    };
}

test("fresh sessions freeze a neutralized primer and reuse it unchanged", async () => {
    const f = fixture();
    const first = f.event();
    await f.controller.onContext(first);
    const text = first.messages[0].content[0].text;
    expect(text).toContain("<memento-project-primer>");
    expect(text).toContain("agy, useful,");
    expect(text).not.toContain("<memento-digest>");
    expect(f.values.get("memento/primer/new")).toBe(text);
    f.candidates.length = 0;
    const second = f.event();
    await f.controller.onContext(second);
    expect(second.messages[0].content[0].text).toBe(text);
    await f.controller.onContext(second);
    expect(second.messages[0].content).toHaveLength(2);
    expect(f.calls).toEqual(["list", "stats:recent", "stats:useful", "digest:useful:4000"]);
    expect(f.activity[0].detail).toEqual({ chars: text.length, segments: 1, outcome: "injected" });
});

test("claimed continuations, forks, old history and prior state freeze empty", async () => {
    for (const condition of ["parent", "fork", "history", "cursor", "unconfirmed"]) {
        const f = fixture();
        if (condition === "parent") { f.values.set("memento/parent/new", "parent"); }
        if (condition === "fork") { f.setFork({ sessionID: "source" }); }
        if (condition === "history") { f.setHistory([{ type: "user" }, { type: "assistant" }, { type: "user" }]); }
        if (condition === "cursor") { f.values.set("memento/ingest/new", "old"); }
        if (condition === "unconfirmed") { f.values.delete("memento/claimed/new"); }
        const event = f.event();
        await f.controller.onContext(event);
        expect(event.messages[0].content).toHaveLength(1);
        expect(f.values.get("memento/primer/new")).toBe("");
        expect(f.calls).toEqual([]);
        expect(f.activity[0].detail.outcome).toBe("none");
    }
});

test("no qualifying candidate and stale candidates freeze empty", async () => {
    for (const stale of [false, true]) {
        const f = fixture();
        if (stale) {
            for (const candidate of f.candidates) { candidate.last_active_at = new Date(Date.now() - 73 * 3600000).toISOString(); }
        } else {
            f.candidates.splice(1);
        }
        await f.controller.onContext(f.event());
        expect(f.values.get("memento/primer/new")).toBe("");
        expect(f.calls).toEqual(stale ? ["list"] : ["list", "stats:recent"]);
    }
});

test("primer failure logs and freezes empty", async () => {
    const f = fixture();
    f.fail();
    await f.controller.onContext(f.event());
    expect(f.values.get("memento/primer/new")).toBe("");
    expect(f.errors).toHaveLength(1);
    expect(f.activity[0].detail.outcome).toBe("none");
});
