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
