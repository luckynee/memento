import { describe, expect, test } from "bun:test";
import { MementoClient, MementoError } from "../src/client";

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
        await client.putSession({ ...identity, parent: "parent", require_parent: true });
        expect(await client.releaseSession(identity)).toEqual({ released: true, removed: [{ harness: "h", session: "s" }] });
        expect(calls).toEqual([
            { path: "/v1/sessions", method: "PUT", body: { ...identity, parent: "parent", require_parent: true } },
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
});
