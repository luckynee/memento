import { expect, test } from "bun:test";
import type { Plugin } from "@opencode/plugin";
import plugin, { projectKey } from "../src/index";

test("uses the canonical project path when available", () => {
    expect(projectKey({ id: "id", canonical: "/checkout" })).toBe("/checkout");
    expect(projectKey({ id: "id", canonical: "" })).toBe("id");
    expect(projectKey({ id: "id", canonical: null })).toBe("id");
    expect(projectKey({ id: "id" })).toBe("id");
});

test("step ended dispatches ingestion and cleanup stops the subscription", async () => {
    const values = new Map<string, unknown>();
    const ingests: unknown[] = [];
    const hooks: string[] = [];
    const disposedHooks: string[] = [];
    let subscriptionTypes: readonly string[] = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url, init) => {
        if (String(url).endsWith("/v1/continuations/claim")) {
            return Response.json({ claimed: false });
        }
        if (String(url).endsWith("/v1/ingest")) {
            ingests.push(JSON.parse(String(init?.body)));
            return Response.json({ segments: 1, skipped: 0 });
        }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false },
        location: { project: { id: "p" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
        },
        session: {
            async get() { return { projectID: "p" }; },
            async context() { return [{ id: "u", type: "user", time: { created: 0 }, text: "hello" }]; },
            async hook(name: string) {
                hooks.push(name);
                return { async dispose() { disposedHooks.push(name); } };
            },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { types: readonly string[]; signal: AbortSignal }) {
                subscriptionTypes = input.types;
                yield { type: "session.step.ended", data: { sessionID: "s", assistantMessageID: "a", finish: "stop" } };
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    try {
        const cleanup = await plugin.setup(ctx);
        await new Promise<void>((resolve, reject) => {
            let attempts = 0;
            const check = () => {
                if (ingests.length) {
                    resolve();
                } else if (++attempts >= 1000) {
                    reject(new Error("ingest did not arrive"));
                } else {
                    setTimeout(check, 1);
                }
            };
            check();
        });
        expect(subscriptionTypes).toContain("session.step.ended");
        expect(ingests).toHaveLength(1);
        expect(hooks).toEqual(["context", "compaction"]);
        if (cleanup) { await cleanup(); }
        expect(disposedHooks).toEqual(["context", "compaction"]);
    } finally {
        globalThis.fetch = originalFetch;
    }
});

test("foreign sessions cannot ingest, rewrite, compact, use commands, or search", async () => {
    const requests: string[] = [];
    const hooks = new Map<string, (event: any) => Promise<void>>();
    const commands = new Map<string, any>();
    let tool: any;
    let lookups = 0;
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url) => {
        requests.push(String(url));
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false },
        location: { project: { id: "p" } },
        storage: {
            async get() { throw new Error("foreign session accessed storage"); },
            async set() { throw new Error("foreign session accessed storage"); },
        },
        session: {
            async get() { lookups++; return { projectID: "other" }; },
            async context() { throw new Error("foreign session loaded context"); },
            async hook(name: string, handler: (event: any) => Promise<void>) {
                hooks.set(name, handler);
                return registration();
            },
            instructions: { entry: { async put() { throw new Error("foreign session wrote instruction"); } } },
        },
        tool: { async transform(callback: (editor: any) => void) { callback({ add(value: unknown) { tool = value; } }); return registration(); } },
        command: { async transform(callback: (editor: any) => void) { callback({ add(value: any) { commands.set(value.name, value); } }); return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                yield { type: "session.step.ended", data: { sessionID: "foreign" } };
                yield { type: "session.execution.succeeded", data: { sessionID: "foreign" } };
                yield { type: "session.model.selected", data: { sessionID: "foreign", model: { providerID: "p", id: "m" } } };
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        const context = { sessionID: "foreign", system: [], messages: [], model: { providerID: "p", id: "m" } };
        const compaction = { ...context };
        await hooks.get("context")!(context);
        await hooks.get("compaction")!(compaction);
        expect(context).toEqual({ sessionID: "foreign", system: [], messages: [], model: { providerID: "p", id: "m" } });
        expect(compaction).toEqual(context);
        for (const name of ["memento-continue", "memento-handoff"]) {
            await expect(commands.get(name).execute({ sessionID: "foreign", prompt: { text: "parent" } })).rejects.toThrow("not enabled");
        }
        expect(await tool.execute({ query: "test" }, { sessionID: "foreign", signal: new AbortController().signal })).toEqual({
            content: "memory_search: memento is not enabled for this project.",
        });
        await cleanup?.();
        cleanup = undefined;
        expect(requests).toEqual([]);
        expect(lookups).toBe(1);
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
    }
});

test("disabled plugin registers nothing", async () => {
    const ctx = { options: { enabled: false } } as unknown as Plugin.Context;
    const cleanup = await plugin.setup(ctx);
    expect(typeof cleanup).toBe("function");
    if (cleanup) { await cleanup(); }
});
