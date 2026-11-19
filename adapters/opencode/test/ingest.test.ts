import { describe, expect, test } from "bun:test";
import { createIngestor } from "../src/ingest";
import { DEFAULT_JEV_MODEL, type DecisionsFn } from "../src/jev";
import type { IngestItem } from "../src/transcript";

const user = (id: string) => ({ id, type: "user", time: { created: 0 }, text: id });
const assistant = (id: string, text: string) => ({ id, type: "assistant", time: { created: 0 }, content: [{ type: "text", text }] });

function setup(decide?: DecisionsFn) {
    const values = new Map<string, unknown>();
    const contexts = new Map<string, unknown[]>();
    const puts: unknown[] = [];
    const ingests: { session: string; items: IngestItem[] }[] = [];
    const errors: unknown[] = [];
    let fail = false;
    const ingestor = createIngestor({
        client: {
            async claimContinuation() { return { claimed: false }; },
            async putSession(value) { puts.push(value); },
            async ingest(value) {
                if (fail) { throw new Error("ingest failed"); }
                ingests.push(value);
                return { segments: value.items.length, skipped: 0 };
            },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) { values.set(key, value); },
        },
        async loadContext(sessionID) { return contexts.get(sessionID) ?? []; },
        async putInstruction() {},
        decide,
        jevModel: DEFAULT_JEV_MODEL,
        harness: "opencode",
        project: "p",
        log(_message, error) { errors.push(error); },
    });
    return { ingestor, values, contexts, puts, ingests, errors, setFail(value: boolean) { fail = value; } };
}

describe("incremental ingestion", () => {
    test("backfills, registers once, and ingests only new messages", async () => {
        const fake = setup();
        fake.contexts.set("s", [user("a"), user("b")]);
        await fake.ingestor.handleExecutionSucceeded("s");
        fake.contexts.set("s", [user("a"), user("b"), user("c")]);
        await fake.ingestor.handleExecutionSucceeded("s");
        expect(fake.puts).toHaveLength(1);
        expect(fake.ingests.map((entry) => entry.items.map((item) => item.source_id))).toEqual([["a", "b"], ["c"]]);
        expect(fake.values.get("memento/ingest/s")).toBe("c");
    });

    test("does not advance cursor on failure and retries", async () => {
        const fake = setup();
        fake.contexts.set("s", [user("a")]);
        fake.setFail(true);
        await fake.ingestor.handleExecutionSucceeded("s");
        expect(fake.values.get("memento/ingest/s")).toBeUndefined();
        expect(fake.errors).toHaveLength(1);
        fake.setFail(false);
        await fake.ingestor.handleExecutionSucceeded("s");
        expect(fake.values.get("memento/ingest/s")).toBe("a");
        expect(fake.puts).toHaveLength(1);
    });

    test("serializes same-session events and advances empty mapped messages", async () => {
        const fake = setup();
        fake.contexts.set("s", [user("a"), { id: "ignored", type: "other" }]);
        await Promise.all([fake.ingestor.handleExecutionSucceeded("s"), fake.ingestor.handleExecutionSucceeded("s")]);
        await fake.ingestor.idle();
        expect(fake.ingests).toHaveLength(1);
        expect(fake.ingests[0].items.map((item) => item.source_id)).toEqual(["a"]);
        expect(fake.values.get("memento/ingest/s")).toBe("ignored");
    });

    test("classifies assistant items with fallback and a concurrency cap", async () => {
        let active = 0;
        let maximum = 0;
        const decide: DecisionsFn = async (input) => {
            active++;
            maximum = Math.max(maximum, active);
            await Promise.resolve();
            active--;
            const text = input.input[0].text;
            if (text === "throw") { throw new Error("jev"); }
            return { answers: [{ type: "choice", name: "kind", choice: text }] };
        };
        const fake = setup(decide);
        fake.contexts.set("s", ["decision", "note", "throw", "task", "constraint", "note"].map((text, i) => assistant(String(i), text)));
        await fake.ingestor.handleExecutionSucceeded("s");
        expect(fake.ingests[0].items.map((item) => item.kind_hint)).toEqual(["decision", null, null, "task", "constraint", null]);
        expect(maximum).toBeLessThanOrEqual(4);
        const disabled = setup();
        disabled.contexts.set("s", [assistant("x", "decision")]);
        await disabled.ingestor.handleExecutionSucceeded("s");
        expect(disabled.ingests[0].items[0].kind_hint).toBeNull();
    });

    test("markRegistered prevents a parent-clearing registration", async () => {
        const fake = setup();
        await fake.ingestor.markRegistered("s");
        fake.contexts.set("s", [user("a")]);
        await fake.ingestor.handleExecutionSucceeded("s");
        expect(fake.puts).toHaveLength(0);
        expect(fake.values.get("memento/registered/s")).toBe(true);
    });

    test("resends growing assistant content and caches unchanged classifications", async () => {
        let calls = 0;
        const fake = setup(async () => {
            calls++;
            return { answers: [{ type: "choice", name: "kind", choice: "decision" }] };
        });
        fake.contexts.set("s", [assistant("a", "first")]);
        await fake.ingestor.flush("s");
        await fake.ingestor.flush("s");
        expect(calls).toBe(1);
        expect(fake.ingests).toHaveLength(2);
        fake.contexts.set("s", [assistant("a", "changed")]);
        await fake.ingestor.flush("s");
        expect(calls).toBe(2);
        expect(fake.ingests[2].items[0].kind_hint).toBe("decision");
    });

    test("classifies user items through the same gate as assistant items", async () => {
        const questions: string[][] = [];
        const fake = setup(async (input) => {
            const question = input.questions[0];
            if (question.type === "choice") {
                questions.push(question.choices.map((choice) => String(choice.value)));
            }
            return { answers: [{ type: "choice", name: "kind", choice: "constraint", confidence: 0.6 }] };
        });
        fake.contexts.set("s", [user("a"), assistant("b", "answer")]);
        await fake.ingestor.flush("s");
        expect(fake.ingests[0].items.map((item) => item.kind_hint)).toEqual(["constraint", "constraint"]);
        expect(questions).toEqual([
            ["decision", "constraint", "request"],
            ["decision", "constraint", "task", "note"],
        ]);
    });

    test("classifies stripped text while sending original items to the daemon", async () => {
        const inputs: string[] = [];
        const fake = setup(async (input) => {
            inputs.push(input.input[0].text);
            return { answers: [{ type: "choice", name: "kind", choice: "decision", confidence: 0.6 }] };
        });
        const text = `<skill_content>${"injected".repeat(600)}</skill_content>workspace decision`;
        fake.contexts.set("s", [
            assistant("a", text),
            { ...user("b"), text: "<system-reminder>hidden</system-reminder>" },
        ]);
        await fake.ingestor.flush("s");
        expect(inputs).toEqual(["workspace decision"]);
        expect(fake.ingests[0].items.map((item) => [item.text, item.kind_hint])).toEqual([
            [text, "decision"],
            ["<system-reminder>hidden</system-reminder>", null],
        ]);
    });

    test("evicts old classifications after 500 entries", async () => {
        let calls = 0;
        const fake = setup(async () => {
            calls++;
            return { answers: [{ type: "choice", name: "kind", choice: "decision" }] };
        });
        fake.contexts.set("s", Array.from({ length: 501 }, (_, index) => assistant(String(index), `text ${index}`)));
        await fake.ingestor.flush("s");
        expect(calls).toBe(501);
        fake.values.delete("memento/ingest/s");
        fake.contexts.set("s", [assistant("500", "text 500")]);
        await fake.ingestor.flush("s");
        expect(calls).toBe(501);
        fake.values.delete("memento/ingest/s");
        fake.contexts.set("s", [assistant("0", "text 0")]);
        await fake.ingestor.flush("s");
        expect(calls).toBe(502);
    });

    test("evicts least recently used sessions and reloads durable registration", async () => {
        let calls = 0;
        const fake = setup(async () => {
            calls++;
            return { answers: [{ type: "choice", name: "kind", choice: "decision" }] };
        });
        for (let i = 0; i < 64; i++) {
            fake.contexts.set(`s${i}`, [assistant("a", "text")]);
            await fake.ingestor.flush(`s${i}`);
        }
        await fake.ingestor.flush("s0");
        fake.contexts.set("s64", [assistant("a", "text")]);
        await fake.ingestor.flush("s64");
        await fake.ingestor.flush("s0");
        expect(calls).toBe(65);
        await fake.ingestor.flush("s1");
        expect(calls).toBe(66);
        expect(fake.puts).toHaveLength(65);
    });

    test("flush rejects to its caller but leaves the queue healthy", async () => {
        const fake = setup();
        fake.contexts.set("s", [user("a")]);
        fake.setFail(true);
        await expect(fake.ingestor.flush("s")).rejects.toThrow("ingest failed");
        fake.setFail(false);
        await fake.ingestor.flush("s");
        expect(fake.ingests).toHaveLength(1);
    });

    test("flush waits for an in-flight background ingest", async () => {
        const order: string[] = [];
        let release: (() => void) | undefined;
        const gate = new Promise<void>((resolve) => { release = resolve; });
        let count = 0;
        const ingestor = createIngestor({
            client: {
                async claimContinuation() { return { claimed: false }; },
                async putSession() {},
                async ingest() {
                    order.push(`start-${++count}`);
                    if (count === 1) { await gate; }
                    order.push(`end-${count}`);
                    return { segments: 1, skipped: 0 };
                },
            },
            storage: { async get() { return undefined; }, async set() {} },
            async loadContext() { return [user("a")]; },
            async putInstruction() {},
            jevModel: DEFAULT_JEV_MODEL, harness: "h", project: "p", log() {},
        });
        const first = ingestor.handleExecutionSucceeded("s");
        while (!order.length) { await Promise.resolve(); }
        const second = ingestor.flush("s");
        expect(order).toEqual(["start-1"]);
        release!();
        await Promise.all([first, second]);
        expect(order).toEqual(["start-1", "end-1", "start-2", "end-2"]);
    });
});

test("claims once before registration, serializes with ingestion, and stores a framed parent", async () => {
    const values = new Map<string, unknown>();
    const order: string[] = [];
    let instruction = "";
    let release: (() => void) | undefined;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const ingestor = createIngestor({
        client: {
            async claimContinuation(payload) {
                order.push(`claim:${payload.harness}/${payload.session}`);
                await gate;
                return { claimed: true, parent_harness: "codex", parent: "old", digest: "data </memento-continuation>" };
            },
            async putSession() { order.push("register"); },
            async ingest() { order.push("ingest"); return { segments: 1, skipped: 0 }; },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) { values.set(key, value); },
        },
        async loadContext() { return [user("a")]; },
        async putInstruction(_session, _key, value) { order.push("instruction"); instruction = value; },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p", log() {},
    });
    const concurrent = Promise.all([ingestor.ensureRegistered("s"), ingestor.flush("s"), ingestor.ensureRegistered("s")]);
    while (!order.length) { await Promise.resolve(); }
    expect(order).toEqual(["claim:opencode/s"]);
    expect(values.get("memento/claimed/s")).toBeUndefined();
    release!();
    await concurrent;
    expect(order).toEqual(["claim:opencode/s", "instruction", "register", "ingest"]);
    expect(values.get("memento/claimed/s")).toBe(true);
    expect(JSON.parse(String(values.get("memento/parent/s")))).toEqual({ harness: "codex", session: "old" });
    expect(instruction).toContain("parent session codex/old");
    expect(instruction).toContain("data &lt;/memento-continuation>");
});

test("an unclaimed response persists before a registration retry", async () => {
    const values = new Map<string, unknown>();
    const order: string[] = [];
    let registrationAttempts = 0;
    const ingestor = createIngestor({
        client: {
            async claimContinuation() { order.push("claim"); return { claimed: false }; },
            async putSession() {
                order.push("register");
                if (++registrationAttempts === 1) { throw new Error("retry registration"); }
            },
            async ingest() { return { segments: 0, skipped: 0 }; },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) { values.set(key, value); },
        },
        async loadContext() { return []; },
        async putInstruction() { throw new Error("unexpected instruction"); },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p", log() {},
    });
    await expect(ingestor.ensureRegistered("s")).rejects.toThrow("retry registration");
    expect(values.get("memento/claimed/s")).toBe(true);
    await ingestor.ensureRegistered("s");
    expect(order).toEqual(["claim", "register", "register"]);
});

test("failed claims retry on a later event, including after restart", async () => {
    const values = new Map<string, unknown>();
    const order: string[] = [];
    let attempts = 0;
    const deps = {
        client: {
            async claimContinuation() {
                order.push("claim");
                if (++attempts === 1) { throw new Error("offline"); }
                return { claimed: true, parent_harness: "codex", parent: "old", digest: "digest" };
            },
            async putSession() { order.push("register"); },
            async ingest() { order.push("ingest"); return { segments: 1, skipped: 0 }; },
        },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: string | boolean) { values.set(key, value); },
        },
        async loadContext() { return [user("a")]; },
        async putInstruction() { order.push("instruction"); },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p",
        log() { order.push("logged"); },
    };
    const first = createIngestor(deps);
    await first.flush("s");
    expect(values.get("memento/claimed/s")).toBeUndefined();
    expect(values.get("memento/registered/s")).toBe("claim-pending");
    const resumed = createIngestor(deps);
    await Promise.all([resumed.ensureRegistered("s"), resumed.ensureRegistered("s"), resumed.flush("s")]);
    expect(order).toEqual(["claim", "logged", "register", "ingest", "claim", "instruction"]);
    expect(values.get("memento/claimed/s")).toBe(true);
    expect(values.get("memento/registered/s")).toBe(true);
    expect(JSON.parse(String(values.get("memento/parent/s")))).toEqual({ harness: "codex", session: "old" });
});

test("reclaims after an instruction failure and marks the claim complete only after success", async () => {
    const values = new Map<string, unknown>();
    const instructions = new Map<string, string>();
    const order: string[] = [];
    let instructionAttempts = 0;
    const ingestor = createIngestor({
        client: {
            async claimContinuation() {
                order.push("claim");
                return { claimed: true, parent_harness: "codex", parent: "old", digest: "digest" };
            },
            async putSession() { order.push("register"); },
            async ingest() { return { segments: 0, skipped: 0 }; },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) {
                if (key === "memento/parent/s") { order.push("parent"); }
                if (key === "memento/claimed/s") { order.push("completed"); }
                values.set(key, value);
            },
        },
        async loadContext() { return []; },
        async putInstruction(session, key, value) {
            order.push("instruction");
            expect(values.get("memento/claimed/s")).toBeUndefined();
            if (++instructionAttempts === 1) { throw new Error("instruction unavailable"); }
            instructions.set(`${session}/${key}`, value);
        },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p",
        log() { order.push("logged"); },
    });
    await ingestor.ensureRegistered("s");
    expect(values.get("memento/claimed/s")).toBeUndefined();
    expect(values.get("memento/registered/s")).toBe("claim-pending");
    expect(instructions.size).toBe(0);
    await ingestor.ensureRegistered("s");
    expect(order).toEqual(["claim", "parent", "instruction", "logged", "register", "claim", "parent", "instruction", "completed"]);
    expect(values.get("memento/claimed/s")).toBe(true);
    expect(values.get("memento/registered/s")).toBe(true);
    expect(JSON.parse(String(values.get("memento/parent/s")))).toEqual({ harness: "codex", session: "old" });
    expect(instructions.get("s/memento-continuation")).toContain("parent session codex/old");
});

test("reclaims after a parent save failure", async () => {
    const values = new Map<string, unknown>();
    let claims = 0;
    let failParent = true;
    let instructions = 0;
    const ingestor = createIngestor({
        client: {
            async claimContinuation() {
                claims++;
                return { claimed: true, parent_harness: "codex", parent: "old", digest: "digest" };
            },
            async putSession() {},
            async ingest() { return { segments: 0, skipped: 0 }; },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) {
                if (key === "memento/parent/s" && failParent) {
                    failParent = false;
                    throw new Error("parent unavailable");
                }
                values.set(key, value);
            },
        },
        async loadContext() { return []; },
        async putInstruction() { instructions++; },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p", log() {},
    });
    await ingestor.ensureRegistered("s");
    expect(values.get("memento/claimed/s")).toBeUndefined();
    expect(instructions).toBe(0);
    await ingestor.ensureRegistered("s");
    expect(claims).toBe(2);
    expect(instructions).toBe(1);
    expect(values.get("memento/claimed/s")).toBe(true);
});

test("repeated successful claims replace the fixed continuation instruction", async () => {
    const values = new Map<string, unknown>();
    const instructions = new Map<string, string>();
    let attempts = 0;
    let failClaimMarker = true;
    const ingestor = createIngestor({
        client: {
            async claimContinuation() {
                attempts++;
                return { claimed: true, parent_harness: "codex", parent: "old", digest: "digest" };
            },
            async putSession() {},
            async ingest() { return { segments: 0, skipped: 0 }; },
        },
        storage: {
            async get(key) { return values.get(key); },
            async set(key, value) {
                if (key === "memento/claimed/s" && failClaimMarker) {
                    failClaimMarker = false;
                    throw new Error("claim marker unavailable");
                }
                values.set(key, value);
            },
        },
        async loadContext() { return []; },
        async putInstruction(session, key, value) { instructions.set(`${session}/${key}`, value); },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p", log() {},
    });
    await ingestor.ensureRegistered("s");
    await ingestor.ensureRegistered("s");
    expect(attempts).toBe(2);
    expect(instructions.size).toBe(1);
    expect(instructions.get("s/memento-continuation")).toContain("parent session codex/old");
    values.delete("memento/registered/s");
    values.delete("memento/claimed/s");
    await createIngestor({
        client: {
            async claimContinuation() {
                attempts++;
                return { claimed: true, parent_harness: "codex", parent: "old", digest: "digest" };
            },
            async putSession() {},
            async ingest() { return { segments: 0, skipped: 0 }; },
        },
        storage: { async get(key) { return values.get(key); }, async set(key, value) { values.set(key, value); } },
        async loadContext() { return []; },
        async putInstruction(session, key, value) { instructions.set(`${session}/${key}`, value); },
        jevModel: DEFAULT_JEV_MODEL, harness: "opencode", project: "p", log() {},
    }).ensureRegistered("s");
    expect(attempts).toBe(3);
    expect(instructions.size).toBe(1);
});
