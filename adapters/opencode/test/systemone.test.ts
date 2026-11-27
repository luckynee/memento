import { afterEach, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createJevGate, DEFAULT_JEV_MODEL, type DecisionsFn } from "../src/jev";
import { createSystemOneDecisions } from "../src/systemone";

const originalFetch = globalThis.fetch;
const originalKey = process.env.MEMENTO_TEST_SYSTEMONE_KEY;
const input: Parameters<DecisionsFn>[0] = {
    model: DEFAULT_JEV_MODEL,
    input: [{ type: "text", text: "first" }, { type: "text", text: "second" }],
    questions: [
        { type: "predicate", name: "relevant", instructions: "Relevant?" },
        { type: "choice", name: "kind", instructions: "Which?", choices: [
            { value: "decision", description: "A decision" }, { value: "note" },
        ] },
        { type: "choice", name: "flag", instructions: "Flag?", choices: [{ value: true }, { value: false }] },
        { type: "predicate", name: "missing", instructions: "Missing?" },
        { type: "predicate", name: "invalid", instructions: "Invalid?" },
    ],
};

afterEach(() => {
    globalThis.fetch = originalFetch;
    if (originalKey === undefined) {
        delete process.env.MEMENTO_TEST_SYSTEMONE_KEY;
    } else {
        process.env.MEMENTO_TEST_SYSTEMONE_KEY = originalKey;
    }
});

test("maps System One requests and answers without authentication", async () => {
    let request: RequestInit | undefined;
    globalThis.fetch = (async (_url, init) => {
        request = init;
        return Response.json({ answers: {
            relevant: { type: "noul", noul: 0.94 },
            kind: { type: "choice", choice: "decision", confidence: 0.8 },
            flag: { type: "choice", choice: "false" },
            invalid: { type: "noul", noul: 3 },
        } });
    }) as typeof fetch;
    const result = await createSystemOneDecisions({ url: "https://example.invalid/v1/systemone", model: "openrouter/typesafe/jev" })(input);
    const headers = new Headers(request?.headers);
    expect(headers.get("authorization")).toBeNull();
    expect(headers.get("content-type")).toBe("application/json");
    expect(headers.get("user-agent")).toBe("memento-opencode/plugin");
    expect(request?.redirect).toBe("error");
    expect(JSON.parse(String(request?.body))).toEqual({
        model: "openrouter/typesafe/jev",
        state: "first\nsecond",
        questions: {
            relevant: { type: "noul", instructions: "Relevant?" },
            kind: { type: "choice", instructions: "Which?", criteria: { decision: "A decision", note: "note" } },
            flag: { type: "choice", instructions: "Flag?", criteria: { true: "true", false: "false" } },
            missing: { type: "noul", instructions: "Missing?" },
            invalid: { type: "noul", instructions: "Invalid?" },
        },
    });
    expect(result.answers).toEqual([
        { type: "predicate", name: "relevant", probability: 0.94 },
        { type: "choice", name: "kind", choice: "decision", confidence: 0.8 },
        { type: "choice", name: "flag", choice: false },
        { type: "error", name: "missing", reason: "missing" },
        { type: "error", name: "invalid", reason: "missing" },
    ]);
});

test("reads an environment key lazily and never exposes it in HTTP errors", async () => {
    const key = "private-value";
    process.env.MEMENTO_TEST_SYSTEMONE_KEY = ` ${key} `;
    const authorizations: (string | null)[] = [];
    globalThis.fetch = (async (_url, init) => {
        authorizations.push(new Headers(init?.headers).get("authorization"));
        return Response.json({ error: { message: key, retryAfterMs: 1 } }, { status: 429, headers: { "Retry-After": "120" } });
    }) as typeof fetch;
    const decide = createSystemOneDecisions({ url: "https://example.invalid", model: "m", apiKeyEnv: "MEMENTO_TEST_SYSTEMONE_KEY" });
    let error: unknown;
    try {
        await decide(input);
    } catch (caught) {
        error = caught;
    }
    expect(authorizations).toEqual([`Bearer ${key}`]);
    expect(error).toMatchObject({ status: 429, reason: "rate_limit", retryAfterMs: 120000 });
    expect(JSON.stringify(Object.getOwnPropertyDescriptors(error))).not.toContain(key);
});

test("reads a trimmed key from a file", async () => {
    const dir = await mkdtemp(join(tmpdir(), "memento-systemone-"));
    try {
        const path = join(dir, "key");
        await writeFile(path, " file-key \n");
        const authorizations: (string | null)[] = [];
        globalThis.fetch = (async (_url, init) => {
            authorizations.push(new Headers(init?.headers).get("authorization"));
            return Response.json({ answers: {} });
        }) as typeof fetch;
        await createSystemOneDecisions({ url: "https://example.invalid", model: "m", apiKeyFile: path })(input);
        expect(authorizations).toEqual(["Bearer file-key"]);
    } finally {
        await rm(dir, { recursive: true, force: true });
    }
});

test("uses body retry information and sanitizes transport errors", async () => {
    globalThis.fetch = (async () => Response.json({ error: { retryAfter: 7 } }, { status: 503 })) as unknown as typeof fetch;
    const decide = createSystemOneDecisions({ url: "https://example.invalid", model: "m" });
    await expect(decide(input)).rejects.toMatchObject({ status: 503, reason: "http_503", retryAfterMs: 7000 });
    process.env.MEMENTO_TEST_SYSTEMONE_KEY = "private-value";
    globalThis.fetch = (async () => { throw new Error("private-value"); }) as unknown as typeof fetch;
    await expect(createSystemOneDecisions({
        url: "https://example.invalid", model: "m", apiKeyEnv: "MEMENTO_TEST_SYSTEMONE_KEY",
    })(input)).rejects.toThrow("System One request failed");
});

test("gate timeout aborts the System One fetch", async () => {
    let expire: (() => void) | undefined;
    let signal: AbortSignal | null | undefined;
    globalThis.fetch = ((_url, init) => {
        signal = init?.signal;
        return new Promise((_resolve, reject) => signal?.addEventListener("abort", () => reject(new Error("aborted"))));
    }) as typeof fetch;
    const decide = createJevGate({
        decide: createSystemOneDecisions({ url: "https://example.invalid", model: "m" }),
        cooldownMs: 100,
        scheduleTimeout(callback) { expire = callback; return () => {}; },
        log() {},
    });
    const result = decide(input);
    await Promise.resolve();
    expect(signal?.aborted).toBe(false);
    expire!();
    await expect(result).rejects.toThrow("Jev request timed out");
    expect(signal?.aborted).toBe(true);
});
