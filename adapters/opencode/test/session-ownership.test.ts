import { expect, test } from "bun:test";
import { createSessionOwnership } from "../src/session-ownership";

test("caches owned and foreign sessions and bounds the cache", async () => {
    const calls = new Map<string, number>();
    const ownsSession = createSessionOwnership({
        async getSessionProjectID(sessionID) {
            calls.set(sessionID, (calls.get(sessionID) ?? 0) + 1);
            return sessionID === "foreign" ? "other" : "project";
        },
        projectID: "project",
        log() {},
    });
    expect(await ownsSession("owned")).toBe(true);
    expect(await ownsSession("owned")).toBe(true);
    expect(await ownsSession("foreign")).toBe(false);
    expect(await ownsSession("foreign")).toBe(false);
    expect(calls.get("owned")).toBe(1);
    expect(calls.get("foreign")).toBe(1);
    for (let index = 0; index < 64; index++) {
        await ownsSession(`session-${index}`);
    }
    expect(await ownsSession("owned")).toBe(true);
    expect(calls.get("owned")).toBe(2);
});

test("fails closed without caching lookup failures", async () => {
    let calls = 0;
    const errors: unknown[] = [];
    const ownsSession = createSessionOwnership({
        async getSessionProjectID() {
            if (++calls === 1) {
                throw new Error("unavailable");
            }
            return "project";
        },
        projectID: "project",
        log(message, error) { errors.push([message, error]); },
    });
    expect(await ownsSession("session")).toBe(false);
    expect(errors).toHaveLength(1);
    expect(await ownsSession("session")).toBe(true);
    expect(await ownsSession("session")).toBe(true);
    expect(calls).toBe(2);
});

test("shares concurrent lookups for a session", async () => {
    let calls = 0;
    const ownsSession = createSessionOwnership({
        async getSessionProjectID() {
            calls++;
            await Promise.resolve();
            return "project";
        },
        projectID: "project",
        log() {},
    });
    expect(await Promise.all([ownsSession("session"), ownsSession("session")])).toEqual([true, true]);
    expect(calls).toBe(1);
});

test("forget clears cached and pending ownership without restoring stale results", async () => {
    let calls = 0;
    let resolveFirst!: (value: string) => void;
    const firstLookup = new Promise<string>((resolve) => { resolveFirst = resolve; });
    const ownsSession = createSessionOwnership({
        async getSessionProjectID() {
            calls++;
            return calls === 1 ? firstLookup : "other";
        },
        projectID: "project",
        log() {},
    });
    const first = ownsSession("session");
    await Promise.resolve();
    ownsSession.forget("session");
    expect(await ownsSession("session")).toBe(false);
    resolveFirst("project");
    expect(await first).toBe(true);
    expect(await ownsSession("session")).toBe(false);
    expect(calls).toBe(2);
    ownsSession.forget("session");
    expect(await ownsSession("session")).toBe(false);
    expect(calls).toBe(3);
});
