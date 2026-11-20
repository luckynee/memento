import { expect, test } from "bun:test";
import { resolveOptions } from "../src/options";
import { parseJevModel } from "../src/jev";

test("validates adapter options and model references", () => {
    const defaults = resolveOptions({ tailUserTurns: 0, tailCharBudget: -1, digestBudget: 1.5, enabled: "no", harness: "", jevCooldownMs: -1 });
    expect(defaults).toMatchObject({ tailUserTurns: 3, tailCharBudget: 40000, digestBudget: 12000, enabled: true, reconcile: true, jev: true, jevCooldownMs: 600000, harness: "opencode" });
    expect(resolveOptions({ enabled: false, reconcile: false, harness: "custom", tailCharBudget: 99, jevCooldownMs: 0 })).toMatchObject({ enabled: false, reconcile: false, harness: "custom", tailCharBudget: 99, jevCooldownMs: 0 });
    expect(resolveOptions({})).toMatchObject({ subagentMemory: "off", subagentMemoryAgents: {}, subagentDigestBudget: 12000 });
    expect(resolveOptions({ subagentMemory: "digest", subagentDigestBudget: 42 })).toMatchObject({ subagentMemory: "digest", subagentDigestBudget: 42 });
    expect(resolveOptions({
        subagentMemory: "filtered",
        subagentMemoryAgents: { coder: "digest", reviewer: "filtered", private: "off", bad: "on", invalid: null },
    })).toMatchObject({ subagentMemory: "filtered", subagentMemoryAgents: { coder: "digest", reviewer: "filtered", private: "off" } });
    for (const value of [null, "digest", [], true]) {
        expect(resolveOptions({ subagentMemoryAgents: value }).subagentMemoryAgents).toEqual({});
    }
    for (const value of ["on", true, null]) {
        expect(resolveOptions({ subagentMemory: value }).subagentMemory).toBe("off");
    }
    for (const value of [0, -1, 1.5, "42", Infinity]) {
        expect(resolveOptions({ subagentDigestBudget: value }).subagentDigestBudget).toBe(12000);
    }
    expect(parseJevModel("provider/model")).toEqual({ providerID: "provider", id: "model" });
    expect(parseJevModel({ providerID: "p", id: "m", variant: "v" })).toEqual({ providerID: "p", id: "m", variant: "v" });
    expect(parseJevModel("bad")).toEqual({ providerID: "opencode", id: "jev-1.13-free" });
});
