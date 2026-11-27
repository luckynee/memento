import { describe, expect, test } from "bun:test";
import { MementoClient, MementoError } from "../src/client";

function refused(): Error {
    return Object.assign(new Error("connection refused"), { code: "ECONNREFUSED" });
}

describe("daemon client", () => {
    test("sets optional bearer authentication and sends JSON", async () => {
        const calls: RequestInit[] = [];
        const fetcher = (async (_url, init) => {
            calls.push(init ?? {});
            return Response.json({ segments: 0, skipped: 0 });
        }) as typeof fetch;
        await new MementoClient({ fetch: fetcher, token: "token" }).ingest({ harness: "h", project: "p", session: "s", items: [] });
        await new MementoClient({ fetch: fetcher }).health();
        expect((calls[0].headers as Record<string, string>).Authorization).toBe("Bearer token");
        expect((calls[1].headers as Record<string, string>).Authorization).toBeUndefined();
        expect(JSON.parse(calls[0].body as string).session).toBe("s");
    });

    test("encodes digest query parameters", async () => {
        let request = "";
        const fetcher = (async (url) => { request = String(url); return Response.json({ text: "d", segments: 1 }); }) as typeof fetch;
        await new MementoClient({ url: "http://example.invalid/", fetch: fetcher }).digest({ harness: "h", project: "a & b", session: "x/y", budget: 4 });
        const url = new URL(request);
        expect(url.pathname).toBe("/v1/digest");
        expect(url.searchParams.get("project")).toBe("a & b");
        expect(url.searchParams.get("session")).toBe("x/y");
        expect(url.searchParams.has("profile")).toBe(false);
        await new MementoClient({ fetch: fetcher }).digest({ harness: "h", project: "p", session: "s", budget: 4, profile: "subagent" });
        expect(new URL(request).searchParams.get("profile")).toBe("subagent");
    });

    test("lists sessions with encoded harness and project", async () => {
        let request = "";
        const client = new MementoClient({
            fetch: (async (url) => {
                request = String(url);
                return Response.json({ sessions: [{ harness: "open/code", project: "a & b", session: "s" }] });
            }) as typeof fetch,
        });
        expect(await client.listSessions("open/code", "a & b")).toEqual({
            sessions: [{ harness: "open/code", project: "a & b", session: "s" }],
        });
        const url = new URL(request);
        expect(url.pathname).toBe("/v1/sessions");
        expect(url.searchParams.get("harness")).toBe("open/code");
        expect(url.searchParams.get("project")).toBe("a & b");
    });

    test("sends continuation requests and reads pair-based lineage", async () => {
        const calls: { path: string; method: string | undefined; body: unknown }[] = [];
        const fetcher = (async (url, init) => {
            calls.push({ path: new URL(String(url)).pathname, method: init?.method, body: init?.body ? JSON.parse(String(init.body)) : undefined });
            return Response.json(calls.length === 3 ? { lineage: [{ harness: "codex", session: "old" }] } : { claimed: false });
        }) as typeof fetch;
        const client = new MementoClient({ fetch: fetcher });
        await client.claimContinuation({ project: "p", harness: "opencode", session: "s" });
        await client.putContinuation({ project: "p", parent_harness: "opencode", parent_session: "s", target_harness: "codex" });
        const lineage: { lineage: { harness: string; session: string }[] } = await client.lineage("open/code", "s/x");
        expect(calls).toEqual([
            { path: "/v1/continuations/claim", method: "POST", body: { project: "p", harness: "opencode", session: "s" } },
            { path: "/v1/continuations", method: "PUT", body: { project: "p", parent_harness: "opencode", parent_session: "s", target_harness: "codex" } },
            { path: "/v1/sessions/open%2Fcode/s%2Fx/lineage", method: "GET", body: undefined },
        ]);
        expect(lineage.lineage).toEqual([{ harness: "codex", session: "old" }]);
    });

    test("sends release and parent-required registration requests", async () => {
        const calls: { path: string; method: string | undefined; body: unknown }[] = [];
        const client = new MementoClient({
            fetch: (async (url, init) => {
                calls.push({ path: new URL(String(url)).pathname, method: init?.method, body: JSON.parse(String(init?.body)) });
                return Response.json({ released: true, removed: [{ harness: "h", session: "s" }] });
            }) as typeof fetch,
        });
        const identity = { harness: "h", project: "p", session: "s" };
        await client.putSession({ ...identity, parent: "parent", parent_cutoff: "2026-11-20T12:00:00.000Z", require_parent: true });
        expect(await client.releaseSession(identity)).toEqual({ released: true, removed: [{ harness: "h", session: "s" }] });
        expect(calls).toEqual([
            { path: "/v1/sessions", method: "PUT", body: { ...identity, parent: "parent", parent_cutoff: "2026-11-20T12:00:00.000Z", require_parent: true } },
            { path: "/v1/sessions/release", method: "POST", body: identity },
        ]);
    });

    test("sends the optional injected marker", async () => {
        let body: unknown;
        const fetcher = (async (_url, init) => {
            body = JSON.parse(String(init?.body));
            return Response.json({ segments: 0, skipped: 1 });
        }) as typeof fetch;
        await new MementoClient({ fetch: fetcher }).ingest({
            harness: "h", project: "p", session: "s",
            items: [{ source_id: "i", ts: "t", role: "user", text: "generated", kind_hint: null, tool: null, injected: true }],
        });
        expect(body).toMatchObject({ items: [{ injected: true }] });
    });

    test("maps JSON and non-JSON HTTP errors", async () => {
        const jsonClient = new MementoClient({ fetch: (async () => Response.json({ error: { code: "bad", message: "details" } }, { status: 400 })) as unknown as typeof fetch });
        try {
            await jsonClient.health();
            throw new Error("expected failure");
        } catch (error) {
            expect(error).toBeInstanceOf(MementoError);
            expect(error).toMatchObject({ status: 400, code: "bad", message: "details" });
        }
        const plainClient = new MementoClient({ fetch: (async () => new Response("oops", { status: 500 })) as unknown as typeof fetch });
        await expect(plainClient.health()).rejects.toMatchObject({ status: 500, code: "http_500" });
    });

    test("maps network failures and timeout", async () => {
        const offline = new MementoClient({ fetch: (async () => { throw new Error("offline"); }) as unknown as typeof fetch });
        await expect(offline.health()).rejects.toMatchObject({ code: "unavailable", message: "offline" });
        const fetcher = (async (_url, init) => new Promise((_resolve, reject) => {
            init?.signal?.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
        })) as typeof fetch;
        const client = new MementoClient({ fetch: fetcher, timeoutMs: 5 });
        await expect(client.health()).rejects.toMatchObject({ code: "timeout" });
    });

    test("deduplicates concurrent startup and retries after health succeeds", async () => {
        let running = false;
        let starts = 0;
        const events: string[] = [];
        const client = new MementoClient({
            autostart: true,
            mementoBin: "/scratch/memento",
            startDaemon: (binary, url) => {
                expect([binary, url]).toEqual(["/scratch/memento", "http://127.0.0.1:7377"]);
                starts++;
                running = true;
            },
            activity: { record(entry) { events.push(String(entry.detail?.outcome)); } },
            fetch: (async (url) => {
                if (!running) {
                    throw refused();
                }
                return Response.json(new URL(String(url)).pathname === "/health" ? { ok: true } : { text: "digest", segments: 1 });
            }) as typeof fetch,
        });
        const payload = { harness: "h", project: "p", session: "s", budget: 10 };
        const results = await Promise.all([client.digest(payload), client.digest(payload)]);
        expect(results.map((result) => result.text)).toEqual(["digest", "digest"]);
        expect(starts).toBe(1);
        expect(events).toEqual(["attempt", "ready"]);
    });

    test("startup timeout degrades and cooldown suppresses another spawn", async () => {
        let starts = 0;
        const events: string[] = [];
        const client = new MementoClient({
            autostart: true,
            mementoBin: "/scratch/memento",
            startDaemon: () => { starts++; },
            startupWaitMs: 10,
            startupPollMs: 2,
            activity: { record(entry) { events.push(String(entry.detail?.outcome)); } },
            fetch: (async () => { throw refused(); }) as unknown as typeof fetch,
        });
        const payload = { harness: "h", project: "p", session: "s", budget: 10 };
        await expect(client.digest(payload)).rejects.toMatchObject({ code: "unavailable" });
        await expect(client.digest(payload)).rejects.toMatchObject({ code: "unavailable" });
        expect(starts).toBe(1);
        expect(events).toEqual(["attempt", "failed"]);
    });

    test("background ingest does not wait for daemon startup", async () => {
        let starts = 0;
        const client = new MementoClient({
            autostart: true,
            mementoBin: "/scratch/memento",
            startDaemon: () => { starts++; },
            startupWaitMs: 10,
            startupPollMs: 2,
            fetch: (async () => { throw refused(); }) as unknown as typeof fetch,
        });
        await expect(client.ingest({ harness: "h", project: "p", session: "s", items: [] }, false)).rejects.toMatchObject({ code: "unavailable" });
        expect(starts).toBe(1);
    });

    test("disabled autostart, relative binaries, and HTTP errors never spawn", async () => {
        let starts = 0;
        const startDaemon = () => { starts++; };
        const offline = (async () => { throw refused(); }) as unknown as typeof fetch;
        await expect(new MementoClient({ mementoBin: "/scratch/memento", startDaemon, fetch: offline }).health()).rejects.toMatchObject({ code: "unavailable" });
        await expect(new MementoClient({ autostart: true, mementoBin: "memento", startDaemon, fetch: offline }).health()).rejects.toMatchObject({ code: "unavailable" });
        await expect(new MementoClient({ autostart: true, mementoBin: "/scratch/memento", startDaemon, fetch: (async () => { throw new Error("bad JSON"); }) as unknown as typeof fetch }).health()).rejects.toMatchObject({ code: "unavailable" });
        await expect(new MementoClient({ autostart: true, mementoBin: "/scratch/memento", startDaemon, fetch: (async () => new Response("", { status: 500 })) as unknown as typeof fetch }).health()).rejects.toMatchObject({ code: "http_500" });
        expect(starts).toBe(0);
    });
});

test("ingest requests use the longer ingest timeout while other requests keep the default", async () => {
    const seen: number[] = [];
    const realTimeout = AbortSignal.timeout;
    AbortSignal.timeout = ((ms: number) => { seen.push(ms); return realTimeout.call(AbortSignal, ms); }) as typeof AbortSignal.timeout;
    try {
        const client = new MementoClient({ fetch: (async () => Response.json({ segments: 0, skipped: 0, ok: true })) as unknown as typeof fetch });
        await client.ingest({ harness: "h", project: "p", session: "s", items: [] });
        await client.health();
    } finally {
        AbortSignal.timeout = realTimeout;
    }
    expect(seen).toEqual([60000, 10000]);
});
