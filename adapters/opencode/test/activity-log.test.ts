import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createActivityLog } from "../src/activity-log";

test("serializes activity records and filters detail fields", async () => {
    const directory = await mkdtemp(join(tmpdir(), "memento-activity-"));
    try {
        const path = join(directory, "state", "activity.jsonl");
        const log = createActivityLog({ path, now: () => 123 });
        log.record({ level: "info", event: "ingest", session: "ses_123", project: "/project", detail: { items: 2, model: "provider/model", prompt: "private" } });
        log.record({ level: "warn", event: "error", message: "failure" });
        await log.flush();
        const entries = (await readFile(path, "utf8")).trim().split("\n").map((line) => JSON.parse(line));
        expect(entries).toEqual([
            { ts: 123, level: "info", event: "ingest", session: "ses_123", project: "/project", detail: { items: 2, model: "provider/model" } },
            { ts: 123, level: "warn", event: "error", message: "failure" },
        ]);
    } finally {
        await rm(directory, { recursive: true, force: true });
    }
});

test("rotates an oversized file before appending and overwrites the previous rotation", async () => {
    const directory = await mkdtemp(join(tmpdir(), "memento-activity-"));
    try {
        const path = join(directory, "activity.jsonl");
        await writeFile(path, "previous content");
        await writeFile(`${path}.1`, "old rotation");
        const log = createActivityLog({ path, maxBytes: 1, now: () => 1 });
        log.record({ level: "info", event: "register" });
        await log.flush();
        expect(await readFile(`${path}.1`, "utf8")).toBe("previous content");
        expect(JSON.parse(await readFile(path, "utf8"))).toEqual({ ts: 1, level: "info", event: "register" });
    } finally {
        await rm(directory, { recursive: true, force: true });
    }
});

test("swallows write and synchronous preparation failures", async () => {
    const directory = await mkdtemp(join(tmpdir(), "memento-activity-"));
    try {
        const log = createActivityLog({ path: directory });
        expect(() => log.record({ level: "info", event: "register" })).not.toThrow();
        await expect(log.flush()).resolves.toBeUndefined();
        const clockFailure = createActivityLog({ path: join(directory, "activity"), now: () => { throw new Error("clock"); } });
        expect(() => clockFailure.record({ level: "info", event: "register" })).not.toThrow();
        await expect(clockFailure.flush()).resolves.toBeUndefined();
    } finally {
        await rm(directory, { recursive: true, force: true });
    }
});

test("truncates error messages and removes credential URLs", async () => {
    const directory = await mkdtemp(join(tmpdir(), "memento-activity-"));
    try {
        const path = join(directory, "activity.jsonl");
        const log = createActivityLog({ path });
        log.record({ level: "error", event: "error", message: "x".repeat(400) });
        log.record({ level: "error", event: "error", message: "failed https://user:secret@example.test/path" });
        await log.flush();
        const [entry, sensitive] = (await readFile(path, "utf8")).trim().split("\n").map((line) => JSON.parse(line));
        expect(entry.message).toHaveLength(300);
        expect(sensitive.message).toBe("[redacted]");
    } finally {
        await rm(directory, { recursive: true, force: true });
    }
});
