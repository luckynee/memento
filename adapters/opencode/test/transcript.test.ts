import { describe, expect, test } from "bun:test";
import { mapMessage, selectNewMessages } from "../src/transcript";

describe("transcript mapping", () => {
    test("maps user and assistant text, ignores reasoning and unfinished tools", () => {
        expect(mapMessage({ id: "u", type: "user", time: { created: 0 }, text: " hello " })).toEqual([
            { source_id: "u", ts: "1970-01-01T00:00:00.000Z", role: "user", text: " hello ", kind_hint: null, tool: null },
        ]);
        expect(mapMessage({
            id: "a", type: "assistant", time: { created: 1000 }, content: [
                { type: "reasoning", text: "secret" },
                { type: "text", text: "answer" },
                { type: "text", text: " " },
                { type: "tool", id: "x", name: "read", state: { status: "streaming", input: "" } },
                { type: "tool", id: "y", name: "read", state: { status: "running", input: {} } },
            ],
        })).toEqual([
            { source_id: "a:text:1", ts: "1970-01-01T00:00:01.000Z", role: "assistant", text: "answer", kind_hint: null, tool: null },
        ]);
        expect(mapMessage({ id: "u", type: "user", time: { created: 0 }, text: " " })).toEqual([]);
        expect(mapMessage({ id: "other", type: "system", time: { created: 0 } })).toEqual([]);
        expect(mapMessage({ id: "synthetic", type: "synthetic", time: { created: 0 }, text: "generated" })).toEqual([]);
        expect(mapMessage({ id: "bad", type: "user", time: { created: NaN }, text: "x" })).toEqual([]);
    });

    test("maps completed and failed tools including files and timestamps", () => {
        const items = mapMessage({
            id: "a", type: "assistant", time: { created: 1000 }, content: [
                { type: "tool", id: "c", name: "edit", time: { created: 2000 }, state: {
                    status: "completed", input: { path: "a" }, content: [
                        { type: "text", text: "done" },
                        { type: "file", uri: "file:a", name: "a" },
                        { type: "file", uri: "file:b" },
                    ],
                } },
                { type: "tool", id: "e", name: "shell", state: {
                    status: "error", input: {}, error: { message: "failed" }, content: [{ type: "text", text: "details" }],
                } },
            ],
        });
        expect(items).toEqual([
            { source_id: "a:tool:c", ts: "1970-01-01T00:00:02.000Z", role: "tool", text: "", kind_hint: null, tool: { name: "edit", input: { path: "a" }, output: "done\n[file a]\n[file file:b]", ok: true } },
            { source_id: "a:tool:e", ts: "1970-01-01T00:00:01.000Z", role: "tool", text: "", kind_hint: null, tool: { name: "shell", input: {}, output: "failed\ndetails", ok: false } },
        ]);
    });

    test("selects after a cursor or backfills missing cursors", () => {
        const messages = [{ id: "a" }, { id: "b" }, { type: "unknown" }];
        expect(selectNewMessages(messages, "a")).toEqual({ messages: messages.slice(1), nextCursor: "b" });
        expect(selectNewMessages(messages)).toEqual({ messages, nextCursor: "b" });
        expect(selectNewMessages(messages, "gone")).toEqual({ messages, nextCursor: "b" });
        expect(selectNewMessages([])).toEqual({ messages: [], nextCursor: undefined });
        const growing = [{ id: "a", type: "user" }, { id: "b", type: "assistant" }, { id: "c", type: "user" }];
        expect(selectNewMessages(growing, "a").messages).toEqual(growing.slice(1));
        expect(selectNewMessages(growing, "b").messages).toEqual(growing.slice(1));
    });
});
