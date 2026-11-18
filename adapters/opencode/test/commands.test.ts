import { expect, test } from "bun:test";
import { createContinueCommand } from "../src/commands";

test("continues from the first parent token and propagates validation errors", async () => {
    const calls: unknown[] = [];
    const command = createContinueCommand({
        client: {
            async putSession(value) { calls.push(["put", value]); },
            async digest(value) { calls.push(["digest", value]); return { text: "digest", segments: 1 }; },
        },
        async markRegistered(sessionID) { calls.push(["mark", sessionID]); },
        async putInstruction(sessionID, key, value) { calls.push(["instruction", sessionID, key, value]); },
        harness: "h", project: "p", digestBudget: 10,
    });
    await command.execute({ sessionID: "child", prompt: { text: " parent extra " } });
    expect(calls).toEqual([
        ["put", { harness: "h", project: "p", session: "child", parent: "parent" }],
        ["mark", "child"],
        ["digest", { harness: "h", project: "p", session: "child", budget: 10 }],
        ["instruction", "child", "memento-continuation", "digest"],
    ]);
    await expect(command.execute({ sessionID: "child", prompt: { text: " " } })).rejects.toThrow("memento-continue requires a parent session id");
    await expect(command.execute({ sessionID: "child", prompt: { text: "child" } })).rejects.toThrow();
});
