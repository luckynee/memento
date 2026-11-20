import { expect, test } from "bun:test";
import { createSessionOwnership } from "../src/session-ownership";

test("caches owned and foreign sessions and bounds the cache", async () => {
    const calls = new Map<string, number>();
    const ownsSession = createSessionOwnership({
        async getSession(sessionID) {
            calls.set(sessionID, (calls.get(sessionID) ?? 0) + 1);
            return { projectID: sessionID === "foreign" ? "other" : "project" };
        },
        projectID: "project",
        directory: "/project",
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
        async getSession() {
            if (++calls === 1) {
                throw new Error("unavailable");
            }
            return { projectID: "project" };
        },
        projectID: "project",
        directory: "/project",
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
        async getSession() {
            calls++;
            await Promise.resolve();
            return { projectID: "project" };
        },
        projectID: "project",
        directory: "/project",
        log() {},
    });
    expect(await Promise.all([ownsSession("session"), ownsSession("session")])).toEqual([true, true]);
    expect(calls).toBe(1);
});

test("forget clears cached and pending ownership without restoring stale results", async () => {
    let calls = 0;
    let resolveFirst!: (value: { projectID: string }) => void;
    const firstLookup = new Promise<{ projectID: string }>((resolve) => { resolveFirst = resolve; });
    const ownsSession = createSessionOwnership({
        async getSession() {
            calls++;
            return calls === 1 ? firstLookup : { projectID: "other" };
        },
        projectID: "project",
        directory: "/project",
        log() {},
    });
    const first = ownsSession("session");
    await Promise.resolve();
    ownsSession.forget("session");
    expect(await ownsSession("session")).toBe(false);
    resolveFirst({ projectID: "project" });
    expect(await first).toBe(true);
    expect(await ownsSession("session")).toBe(false);
    expect(calls).toBe(2);
    ownsSession.forget("session");
    expect(await ownsSession("session")).toBe(false);
    expect(calls).toBe(3);
});

test("resolves owned subagent ancestors and rejects cycles and foreign ancestors", async () => {
    const calls: string[] = [];
    const parents: Record<string, string> = { child: "middle", middle: "root", cycle: "cycle", foreignChild: "foreign" };
    const ownsSession = createSessionOwnership({
        async getSession(sessionID) {
            calls.push(sessionID);
            return { projectID: sessionID === "foreign" ? "other" : "project", parentID: parents[sessionID] };
        },
        projectID: "project",
        directory: "/project",
        log() {},
    });
    expect(await ownsSession("child")).toBe(false);
    expect(await ownsSession.root("child")).toBe("root");
    expect(await ownsSession.root("child")).toBe("root");
    expect(calls.filter((id) => ["child", "middle", "root"].includes(id))).toEqual(["child", "middle", "root"]);
    expect(await ownsSession.root("cycle")).toBeUndefined();
    expect(await ownsSession.root("foreignChild")).toBeUndefined();
});

test("bounds ancestor traversal", async () => {
    const ownsSession = createSessionOwnership({
        async getSession(sessionID) {
            return { projectID: "project", parentID: String(Number(sessionID) + 1) };
        },
        projectID: "project",
        directory: "/project",
        log() {},
    });
    expect(await ownsSession.root("0")).toBeUndefined();
});

test("global ownership requires the same directory with at most one trailing slash ignored", async () => {
    const sessions: Record<string, { projectID: string; location?: { directory?: string } }> = {
        same: { projectID: "global", location: { directory: "/project/" } },
        other: { projectID: "global", location: { directory: "/other" } },
        missingLocation: { projectID: "global" },
        missingDirectory: { projectID: "global", location: {} },
        emptyDirectory: { projectID: "global", location: { directory: "" } },
        multipleSlashes: { projectID: "global", location: { directory: "/project//" } },
        otherProject: { projectID: "other", location: { directory: "/project" } },
    };
    const ownsSession = createSessionOwnership({
        async getSession(sessionID) { return sessions[sessionID]; },
        projectID: "global",
        directory: "/project",
        log() {},
    });
    expect(await ownsSession("same")).toBe(true);
    for (const sessionID of ["other", "missingLocation", "missingDirectory", "emptyDirectory", "multipleSlashes", "otherProject"]) {
        expect(await ownsSession(sessionID)).toBe(false);
    }
    expect(ownsSession.isOwned({ projectID: "global", location: { directory: "/project" } })).toBe(true);
});

test("non-global ownership ignores the session directory", async () => {
    const ownsSession = createSessionOwnership({
        async getSession() { return { projectID: "project", location: { directory: "/other" } }; },
        projectID: "project",
        directory: "/project",
        log() {},
    });
    expect(await ownsSession("session")).toBe(true);
});
