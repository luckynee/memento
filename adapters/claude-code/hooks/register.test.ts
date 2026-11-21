import { expect, test } from "claude-code/testing";

test("registers native tools and passes safe argv to memento", async ($, on) => {
    const registered: string[] = [];
    const calls: (readonly string[])[] = [];
    on("tool.register", ($, event) => {
        registered.push(event.name);
        return { value: {} as never };
    });
    on("session.start", () => ({ cwd: "/work" }));
    on("session.id", () => ({ value: "current-session" }));
    on("process.run", ($, event) => {
        calls.push(event.argv);
        return { value: { exitCode: 0, stdout: " result \n", stderr: "", isStdoutTruncated: false, isStderrTruncated: false } };
    });

    await $.session.start({ surface: "terminal", isInteractive: true, cwd: "/work" });
    expect(registered).toEqual(["memory_search", "memory_digest", "memory_continue", "memory_note"]);
    expect(await $.tool.call({ tool: "mcp__memento__memory_search", query: "-x", k: 70 })).toEqual({ result: "result" });
    expect(await $.tool.call({ tool: "mcp__memento__memory_digest" })).toEqual({ result: "result" });
    expect(await $.tool.call({ tool: "mcp__memento__memory_continue", from_harness: "codex", from_session: "parent" })).toEqual({ result: "result" });
    expect(await $.tool.call({ tool: "mcp__memento__memory_note", kind: "note", text: "-x" })).toEqual({ result: "result" });
    expect(calls).toEqual([
        ["memento", "search", "--framed", "--scope", "project", "--k", "50", "--", "-x"],
        ["memento", "digest", "--framed", "--harness", "claude-code", "--session", "current-session"],
        ["memento", "digest", "--continuation", "--harness", "codex", "--session", "parent"],
        ["memento", "note", "--kind", "note", "--harness", "claude-code", "--session", "current-session", "--", "-x"],
    ]);
});

test("reports process failures and rejects invalid arguments", async ($, on) => {
    on("process.run", () => ({ value: { exitCode: 1, stdout: "", stderr: " unavailable \n", isStdoutTruncated: false, isStderrTruncated: false } }));
    expect(await $.tool.call({ tool: "mcp__memento__memory_search", query: "topic" })).toEqual({ result: "memento search failed: unavailable" });
    expect(await $.tool.call({ tool: "mcp__memento__memory_search", query: 1 })).toEqual({ result: "memory_search failed: query must be a string" });
});

test("reports process startup failure and falls back without a session id", async ($, on) => {
    const calls: (readonly string[])[] = [];
    on("session.id", () => ({ deny: "session unavailable" }));
    on("process.run", ($, event) => {
        calls.push(event.argv);
        return { deny: "program unavailable" };
    });
    const answer = await $.tool.call({ tool: "mcp__memento__memory_note", kind: "note", text: "text" });
    expect(answer.result).toContain("Ensure memento is on PATH.");
    expect(calls).toEqual([["memento", "note", "--kind", "note", "--harness", "claude-code", "--session", "claude-code-notes", "--", "text"]]);
});
