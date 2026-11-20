import { expect, test } from "bun:test";
import type { Plugin } from "@opencode/plugin";
import { frameContinuation } from "../src/commands";
import plugin, { projectKey } from "../src/index";

test("uses the canonical project path when available", () => {
    expect(projectKey({ id: "id", canonical: "/checkout" }, "/location")).toBe("/checkout");
    expect(projectKey({ id: "id", canonical: "" }, "/location")).toBe("id");
    expect(projectKey({ id: "id", canonical: null }, "/location")).toBe("id");
    expect(projectKey({ id: "id" }, "/location")).toBe("id");
    expect(projectKey({ id: "global", canonical: "/shared" }, "/location")).toBe("/location");
});

test("context claims inject once, preserve existing continuation, and coexist with switch rewrites", async () => {
    const values = new Map<string, unknown>();
    const instructions = new Map<string, string>();
    const hooks = new Map<string, (event: any) => Promise<void>>();
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url, init) => {
        const path = new URL(String(url)).pathname;
        if (path === "/v1/continuations/claim") {
            const { session } = JSON.parse(String(init?.body));
            return Response.json(session === "unclaimed"
                ? { claimed: false }
                : { claimed: true, parent_harness: "codex", parent: "parent", digest: "parent digest" });
        }
        if (path === "/v1/digest") {
            return Response.json({ text: "switch digest", segments: 1 });
        }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false, reconcile: false, tailUserTurns: 1 },
        location: { project: { id: "p" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
        },
        session: {
            async get() { return { projectID: "p", time: { created: 0 } }; },
            async context({ sessionID }: { sessionID: string }) {
                return sessionID === "switched"
                    ? [{ type: "assistant", model: { providerID: "p", id: "old" } }]
                    : [];
            },
            async hook(name: string, handler: (event: any) => Promise<void>) { hooks.set(name, handler); return registration(); },
            instructions: { entry: { async put({ sessionID, key, value }: { sessionID: string; key: string; value: string }) {
                expect(key).toBe("memento-continuation");
                instructions.set(sessionID, value);
            } } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        const continuation = frameContinuation("codex", "parent", "parent digest");
        const user = { id: "u", role: "user", content: [{ type: "text", text: "hello" }] };
        const first = { sessionID: "first", model: { providerID: "p", id: "new" }, system: [] as { type: string; text: string }[], messages: [user] };
        await hooks.get("context")!(first);
        expect(instructions.get("first")).toBe(continuation);
        expect(first.messages[0]).not.toBe(user);
        expect(user.content).toEqual([{ type: "text", text: "hello" }]);
        expect(first.messages[0].content).toEqual([{ type: "text", text: continuation }, ...user.content]);
        const second = { ...first, system: [{ type: "text", text: continuation }], messages: [user] };
        await hooks.get("context")!(second);
        expect(second.system).toEqual([{ type: "text", text: continuation }]);
        expect(second.messages).toEqual([user]);
        const unclaimed = { ...first, sessionID: "unclaimed", messages: [user] };
        await hooks.get("context")!(unclaimed);
        expect(unclaimed.messages[0]).toBe(user);
        const existingSystem = { ...first, sessionID: "existing-system", system: [{ type: "text", text: continuation }], messages: [user] };
        await hooks.get("context")!(existingSystem);
        expect(existingSystem.messages[0]).toBe(user);
        const existingMessage = { ...first, sessionID: "existing-message", messages: [{ ...user, content: [{ type: "text", text: continuation }] }] };
        await hooks.get("context")!(existingMessage);
        expect(existingMessage.messages[0].content).toHaveLength(1);
        const noUser = { ...first, sessionID: "no-user", messages: [] };
        await hooks.get("context")!(noUser);
        expect(noUser.system).toEqual([{ type: "text", text: continuation }]);
        const switched = {
            ...first,
            sessionID: "switched",
            messages: [
                { ...user, id: "oldest" },
                { ...user, id: "recent" },
                { ...user, id: "current" },
            ],
        };
        await hooks.get("context")!(switched);
        expect(switched.messages.map((message) => message.id)).toEqual(["recent", "current"]);
        expect(switched.messages[0].content[0].text).toBe(continuation);
        expect(switched.messages[0].content[1].text).toContain("<memento-digest>");
        expect(switched.messages[0].content[1].text).toContain("switch digest");
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
    }
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
        options: { jev: false, reconcile: false },
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
        options: { jev: false, reconcile: false },
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

test("deletion waits for ingest, releases without ownership lookup, and clears session state", async () => {
    const values = new Map<string, unknown>();
    const keys = ["ingest", "registered", "fork", "claimed", "parent", "switch", "subagent"];
    const hooks = new Map<string, (event: any) => Promise<void>>();
    const requests: { path: string; body: any }[] = [];
    let lookups = 0;
    let releaseIngest!: () => void;
    let ingestStarted!: () => void;
    let deletionStarted!: () => void;
    let deletionFinished!: () => void;
    let allowDeletion!: () => void;
    const ingestGate = new Promise<void>((resolve) => { releaseIngest = resolve; });
    const started = new Promise<void>((resolve) => { ingestStarted = resolve; });
    const deleting = new Promise<void>((resolve) => { deletionStarted = resolve; });
    const deletionGate = new Promise<void>((resolve) => { allowDeletion = resolve; });
    const deleted = new Promise<void>((resolve) => { deletionFinished = resolve; });
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url, init) => {
        const path = new URL(String(url)).pathname;
        requests.push({ path, body: init?.body ? JSON.parse(String(init.body)) : undefined });
        if (path === "/v1/continuations/claim") { return Response.json({ claimed: false }); }
        if (path === "/v1/ingest") {
            ingestStarted();
            await ingestGate;
            return Response.json({ segments: 1, skipped: 0 });
        }
        if (path === "/v1/sessions/release") { return Response.json({ released: true, removed: [] }); }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false, reconcile: false, harness: "custom" },
        location: { project: { id: "p", canonical: "/project" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
            async remove(key: string) { values.delete(key); },
        },
        session: {
            async get() { lookups++; return { projectID: "p", time: { created: 0 } }; },
            async context() { return [{ id: "u", type: "user", time: { created: 0 }, text: "hello" }]; },
            async hook(name: string, handler: (event: any) => Promise<void>) { hooks.set(name, handler); return registration(); },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { types: readonly string[]; signal: AbortSignal }) {
                expect(input.types).toContain("session.deleted");
                expect(input.types).not.toContain("session.archived");
                yield { type: "session.step.ended", data: { sessionID: "s" } };
                await started;
                deletionStarted();
                await deletionGate;
                yield { type: "session.deleted", data: { sessionID: "s" }, location: null };
                deletionFinished();
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        await deleting;
        const context = { sessionID: "s", model: { providerID: "p", id: "m" }, system: [], messages: [] };
        await hooks.get("compaction")!(context);
        expect(values.has("memento/switch/s")).toBe(false);
        allowDeletion();
        await Promise.resolve();
        for (const key of keys) { values.set(`memento/${key}/s`, "value"); }
        expect(requests.some((request) => request.path === "/v1/sessions/release")).toBe(false);
        const lookupCount = lookups;
        releaseIngest();
        await deleted;
        expect(requests.filter((request) => request.path === "/v1/sessions/release")).toEqual([
            { path: "/v1/sessions/release", body: { harness: "custom", project: "/project", session: "s" } },
        ]);
        expect(lookups).toBe(lookupCount);
        expect(keys.every((key) => !values.has(`memento/${key}/s`))).toBe(true);
        await hooks.get("context")!(context);
        expect(lookups).toBeGreaterThan(lookupCount);
        expect(values.has("memento/switch/s")).toBe(true);
    } finally {
        allowDeletion();
        releaseIngest();
        await cleanup?.();
        globalThis.fetch = originalFetch;
    }
});

test("release failure is logged and later events are processed", async () => {
    const values = new Map<string, unknown>([["memento/registered/deleted", true]]);
    const requests: string[] = [];
    const warnings: unknown[][] = [];
    let finished!: () => void;
    const done = new Promise<void>((resolve) => { finished = resolve; });
    const originalFetch = globalThis.fetch;
    const originalWarn = console.warn;
    console.warn = (...args: unknown[]) => { warnings.push(args); };
    globalThis.fetch = (async (url) => {
        const path = new URL(String(url)).pathname;
        requests.push(path);
        if (path === "/v1/sessions/release") { return Response.json({ error: { code: "offline", message: "release failed" } }, { status: 503 }); }
        if (path === "/v1/continuations/claim") { return Response.json({ claimed: false }); }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false, reconcile: false },
        location: { project: { id: "p" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
            async remove(key: string) { values.delete(key); },
        },
        session: {
            async get({ sessionID }: { sessionID: string }) {
                if (sessionID === "deleted") { throw new Error("deleted session lookup"); }
                return { projectID: "p", time: { created: 0 } };
            },
            async context() { return []; },
            async hook() { return registration(); },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                yield { type: "session.deleted", data: { sessionID: "deleted" }, location: null };
                yield { type: "session.archived", data: { sessionID: "archived" } };
                yield { type: "session.model.selected", data: { sessionID: "next", model: { providerID: "p", id: "m" } } };
                finished();
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        await done;
        expect(warnings.some((args) => String(args[0]).includes("Failed to release session deleted"))).toBe(true);
        expect(values.has("memento/registered/deleted")).toBe(false);
        expect(values.has("memento/switch/next")).toBe(true);
        expect(requests).toContain("/v1/sessions/release");
        expect(requests.filter((path) => path === "/v1/sessions/release")).toHaveLength(1);
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
        console.warn = originalWarn;
    }
});

test("disabled plugin registers nothing", async () => {
    const ctx = { options: { enabled: false } } as unknown as Plugin.Context;
    const cleanup = await plugin.setup(ctx);
    expect(typeof cleanup).toBe("function");
    if (cleanup) { await cleanup(); }
});

test("subagents skip session processing while search uses the root and forks remain enabled", async () => {
    const values = new Map<string, unknown>();
    const requests: { path: string; body: any }[] = [];
    const hooks = new Map<string, (event: any) => Promise<void>>();
    const commands = new Map<string, any>();
    let tool: any;
    let finished!: () => void;
    const done = new Promise<void>((resolve) => { finished = resolve; });
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url, init) => {
        const path = new URL(String(url)).pathname;
        requests.push({ path, body: init?.body ? JSON.parse(String(init.body)) : undefined });
        if (path === "/v1/continuations/claim") { return Response.json({ claimed: false }); }
        if (path === "/v1/search") { return Response.json({ hits: [] }); }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false, reconcile: false, subagentMemory: "off" },
        location: { project: { id: "p" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
            async remove(key: string) { values.delete(key); },
        },
        session: {
            async get({ sessionID }: { sessionID: string }) {
                if (sessionID === "child") { return { projectID: "p", parentID: "middle" }; }
                if (sessionID === "middle") { return { projectID: "p", parentID: "root" }; }
                if (sessionID === "fork") { return { projectID: "p", fork: { sessionID: "root" }, time: { created: 1 } }; }
                return { projectID: "p", time: { created: 0 } };
            },
            async context() { return []; },
            async hook(name: string, handler: (event: any) => Promise<void>) { hooks.set(name, handler); return registration(); },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform(callback: (editor: any) => void) { callback({ add(value: unknown) { tool = value; } }); return registration(); } },
        command: { async transform(callback: (editor: any) => void) { callback({ add(value: any) { commands.set(value.name, value); } }); return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                yield { type: "session.step.ended", data: { sessionID: "child" } };
                yield { type: "session.execution.succeeded", data: { sessionID: "child" } };
                yield { type: "session.model.selected", data: { sessionID: "child", model: { providerID: "p", id: "m" } } };
                finished();
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        await done;
        const context = { sessionID: "child", system: [], messages: [], model: { providerID: "p", id: "m" } };
        const compaction = { ...context };
        await hooks.get("context")!(context);
        await hooks.get("compaction")!(compaction);
        expect(context).toEqual({ sessionID: "child", system: [], messages: [], model: { providerID: "p", id: "m" } });
        expect(compaction).toEqual(context);
        expect(values.size).toBe(0);
        expect(requests).toEqual([]);
        for (const name of ["memento-continue", "memento-handoff"]) {
            await expect(commands.get(name).execute({ sessionID: "child", prompt: { text: "parent" } })).rejects.toThrow("subagent");
        }
        await tool.execute({ query: "test" }, { sessionID: "child", signal: new AbortController().signal });
        expect(requests).toEqual([{ path: "/v1/search", body: { harness: "opencode", project: "p", session: "root", query: "test", k: 12 } }]);
        await hooks.get("context")!({ ...context, sessionID: "fork" });
        expect(requests.some((request) => request.path === "/v1/sessions" && request.body?.parent === "root")).toBe(true);
        expect(values.has("memento/registered/fork")).toBe(true);
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
    }
});

test("opt-in child context receives the flushed root digest without child registration", async () => {
    const values = new Map<string, unknown>();
    const requests: { path: string; session?: string }[] = [];
    const hooks = new Map<string, (event: any) => Promise<void>>();
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url, init) => {
        const request = new URL(String(url));
        const body = init?.body ? JSON.parse(String(init.body)) : undefined;
        requests.push({ path: request.pathname, session: request.searchParams.get("session") ?? body?.session });
        if (request.pathname === "/v1/continuations/claim") { return Response.json({ claimed: false }); }
        if (request.pathname === "/v1/digest") {
            expect(request.searchParams.get("budget")).toBe("700");
            expect(request.searchParams.get("profile")).toBe("subagent");
            return Response.json({ text: "root memory", segments: 1 });
        }
        if (request.pathname === "/v1/ingest") { return Response.json({ segments: 1, skipped: 0 }); }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false, reconcile: false, subagentMemory: "off", subagentMemoryAgents: { reviewer: "filtered" }, subagentDigestBudget: 700 },
        location: { project: { id: "p" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
        },
        session: {
            async get({ sessionID }: { sessionID: string }) {
                return sessionID === "child" ? { projectID: "p", parentID: "root", agent: "reviewer" } : { projectID: "p", time: { created: 0 } };
            },
            async context({ sessionID }: { sessionID: string }) {
                expect(sessionID).toBe("root");
                return [{ id: "u", type: "user", time: { created: 0 }, text: "parent turn" }];
            },
            async hook(name: string, handler: (event: any) => Promise<void>) { hooks.set(name, handler); return registration(); },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        const first = { sessionID: "child", model: { providerID: "p", id: "m" }, system: [], messages: [{ role: "user", content: [{ type: "text", text: "task" }] }] };
        const second = { ...first, messages: [{ role: "user", content: [{ type: "text", text: "task" }] }] };
        await hooks.get("context")!(first);
        await hooks.get("context")!(second);
        expect(first.messages[0].content[0].text).toContain("root memory");
        expect(second.messages[0].content[0]).toEqual(first.messages[0].content[0]);
        expect(requests.filter((request) => request.path === "/v1/digest")).toEqual([{ path: "/v1/digest", session: "root" }]);
        expect(requests.findIndex((request) => request.path === "/v1/ingest")).toBeLessThan(requests.findIndex((request) => request.path === "/v1/digest"));
        expect(requests.some((request) => request.session === "child")).toBe(false);
        expect([...values.keys()].filter((key) => key.endsWith("/child"))).toEqual(["memento/subagent/child"]);
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
    }
});

test("startup and periodic reconciliation release deleted sessions and subagents, then dispose the timer", async () => {
    const keys = ["ingest", "registered", "fork", "claimed", "parent", "switch"];
    const values = new Map<string, unknown>();
    for (const session of ["missing", "tagged", "child", "unknown", "fork"]) {
        for (const key of keys) { values.set(`memento/${key}/${session}`, true); }
    }
    const releases: any[] = [];
    const requests: string[] = [];
    let done!: () => void;
    const completed = new Promise<void>((resolve) => { done = resolve; });
    let periodic!: () => void;
    let cleared: unknown;
    const timer = {} as ReturnType<typeof setInterval>;
    const originalFetch = globalThis.fetch;
    const originalSetInterval = globalThis.setInterval;
    const originalClearInterval = globalThis.clearInterval;
    globalThis.setInterval = ((callback: () => void, delay: number) => {
        expect(delay).toBe(24 * 60 * 60 * 1000);
        periodic = callback;
        return timer;
    }) as typeof setInterval;
    globalThis.clearInterval = ((handle: unknown) => { cleared = handle; }) as typeof clearInterval;
    globalThis.fetch = (async (url, init) => {
        const request = new URL(String(url));
        requests.push(request.pathname);
        if (request.pathname === "/v1/sessions") {
            requests.push(request.search);
            return Response.json({ sessions: [
                ...["missing", "tagged", "child", "unknown", "fork"].map((session) => ({ harness: "custom", project: "/project", session })),
                { harness: "opencode", project: "/project", session: "other-harness" },
                { harness: "custom", project: "/other-project", session: "other-project" },
            ] });
        }
        if (request.pathname === "/v1/sessions/release") {
            releases.push(JSON.parse(String(init?.body)));
            return Response.json({ released: true, removed: [] });
        }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false, harness: "custom" },
        location: { project: { id: "p", canonical: "/project" } },
        storage: {
            async get(key: string) { return values.get(key); },
            async set(key: string, value: unknown) { values.set(key, value); },
            async remove(key: string) { values.delete(key); },
        },
        session: {
            async get({ sessionID }: { sessionID: string }) {
                if (sessionID === "missing") { throw new Error("Session.NotFoundError"); }
                if (sessionID === "tagged") { throw { _tag: "SessionNotFoundError" }; }
                if (sessionID === "unknown") { throw new Error("server unavailable"); }
                if (sessionID === "child") { return { projectID: "p", parentID: "root" }; }
                done();
                return { projectID: "p", fork: { sessionID: "root" } };
            },
            async context() { return []; },
            async hook() { return registration(); },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        await completed;
        expect(requests).toContain("?harness=custom&project=%2Fproject");
        expect(releases).toEqual([
            { harness: "custom", project: "/project", session: "missing", reason: "deleted" },
            { harness: "custom", project: "/project", session: "tagged", reason: "deleted" },
            { harness: "custom", project: "/project", session: "child", reason: "subagent" },
        ]);
        for (const session of ["missing", "tagged", "child"]) {
            expect(keys.every((key) => !values.has(`memento/${key}/${session}`))).toBe(true);
        }
        for (const session of ["unknown", "fork"]) {
            expect(keys.every((key) => values.has(`memento/${key}/${session}`))).toBe(true);
        }
        await new Promise<void>((resolve) => setTimeout(resolve, 0));
        periodic();
        await Promise.resolve();
        expect(requests.filter((path) => path === "/v1/sessions")).toHaveLength(2);
    } finally {
        await cleanup?.();
        expect(cleared).toBe(timer);
        globalThis.fetch = originalFetch;
        globalThis.setInterval = originalSetInterval;
        globalThis.clearInterval = originalClearInterval;
    }
});

test("disabled reconciliation creates no timer or startup request", async () => {
    const originalFetch = globalThis.fetch;
    const originalSetInterval = globalThis.setInterval;
    let requests = 0;
    let timers = 0;
    globalThis.fetch = (async () => { requests++; return Response.json({ sessions: [] }); }) as unknown as typeof fetch;
    globalThis.setInterval = ((..._args: unknown[]) => { timers++; return {} as ReturnType<typeof setInterval>; }) as unknown as typeof setInterval;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { reconcile: false, jev: false },
        location: { project: { id: "p" } },
        storage: {},
        session: { async hook() { return registration(); } },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        expect(requests).toBe(0);
        expect(timers).toBe(0);
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
        globalThis.setInterval = originalSetInterval;
    }
});

test("global reconciliation releases only subagents from the plugin directory", async () => {
    const releases: unknown[] = [];
    let completed!: () => void;
    const done = new Promise<void>((resolve) => { completed = resolve; });
    const originalFetch = globalThis.fetch;
    globalThis.fetch = (async (url, init) => {
        const request = new URL(String(url));
        if (request.pathname === "/v1/sessions") {
            expect(request.searchParams.get("project")).toBe("/project");
            return Response.json({ sessions: ["foreign", "missing", "local"].map((session) => ({
                harness: "opencode", project: "/project", session,
            })) });
        }
        if (request.pathname === "/v1/sessions/release") {
            releases.push(JSON.parse(String(init?.body)));
            completed();
            return Response.json({ released: true, removed: [] });
        }
        return Response.json({});
    }) as typeof fetch;
    const registration = () => ({ async dispose() {} });
    const ctx = {
        options: { jev: false },
        location: { directory: "/project", project: { id: "global", canonical: "/shared" } },
        storage: { async remove() {} },
        session: {
            async get({ sessionID }: { sessionID: string }) {
                const location = sessionID === "missing" ? undefined
                    : { directory: sessionID === "foreign" ? "/other" : "/project/" };
                return { projectID: "global", parentID: "root", location };
            },
            async hook() { return registration(); },
            instructions: { entry: { async put() {} } },
        },
        tool: { async transform() { return registration(); } },
        command: { async transform() { return registration(); } },
        event: {
            async *subscribe(input: { signal: AbortSignal }) {
                if (!input.signal.aborted) {
                    await new Promise<void>((resolve) => input.signal.addEventListener("abort", () => resolve(), { once: true }));
                }
            },
        },
    } as unknown as Plugin.Context;
    let cleanup: void | (() => void | Promise<void>) = undefined;
    try {
        cleanup = await plugin.setup(ctx);
        await done;
        expect(releases).toEqual([
            { harness: "opencode", project: "/project", session: "local", reason: "subagent" },
        ]);
    } finally {
        await cleanup?.();
        globalThis.fetch = originalFetch;
    }
});
