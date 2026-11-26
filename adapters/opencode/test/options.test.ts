import { expect, test } from "bun:test";
import { resolveOptions } from "../src/options";
import { parseJevModel } from "../src/jev";

test("validates adapter options and model references", () => {
    const defaults = resolveOptions({ tailUserTurns: 0, tailCharBudget: -1, digestBudget: 1.5, enabled: "no", harness: "", jevCooldownMs: -1 });
    expect(defaults).toMatchObject({ tailUserTurns: 3, tailCharBudget: 40000, digestBudget: 12000, enabled: true, reconcile: true, jev: true, jevCooldownMs: 600000, harness: "opencode" });
    expect(resolveOptions({ enabled: false, reconcile: false, harness: "custom", tailCharBudget: 99, jevCooldownMs: 0 })).toMatchObject({ enabled: false, reconcile: false, harness: "custom", tailCharBudget: 99, jevCooldownMs: 0 });
    expect(resolveOptions({})).toMatchObject({ subagentMemory: "digest", subagentMemoryAgents: {}, subagentDigestBudget: 12000 });
    expect(resolveOptions({}).compaction).toBe("native");
    expect(resolveOptions({}).activityLog).toBe(true);
    expect(resolveOptions({ activityLog: false }).activityLog).toBe(false);
    expect(resolveOptions({ activityLog: "/custom/activity.jsonl" }).activityLog).toBe("/custom/activity.jsonl");
    expect(resolveOptions({ activityLog: "" }).activityLog).toBe(true);
    expect(resolveOptions({ compaction: "native" }).compaction).toBe("native");
    expect(resolveOptions({ compaction: "memento" }).compaction).toBe("memento");
    for (const value of ["other", true, null]) {
        expect(resolveOptions({ compaction: value }).compaction).toBe("native");
    }
    expect(resolveOptions({ subagentMemory: "digest", subagentDigestBudget: 42 })).toMatchObject({ subagentMemory: "digest", subagentDigestBudget: 42 });
    expect(resolveOptions({
        subagentMemory: "filtered",
        subagentMemoryAgents: { coder: "digest", reviewer: "filtered", private: "off", bad: "on", invalid: null },
    })).toMatchObject({ subagentMemory: "filtered", subagentMemoryAgents: { coder: "digest", reviewer: "filtered", private: "off" } });
    for (const value of [null, "digest", [], true]) {
        expect(resolveOptions({ subagentMemoryAgents: value }).subagentMemoryAgents).toEqual({});
    }
    for (const value of ["on", true, null]) {
        expect(resolveOptions({ subagentMemory: value }).subagentMemory).toBe("digest");
    }
    for (const value of [0, -1, 1.5, "42", Infinity]) {
        expect(resolveOptions({ subagentDigestBudget: value }).subagentDigestBudget).toBe(12000);
    }
    expect(parseJevModel("provider/model")).toEqual({ providerID: "provider", id: "model" });
    expect(parseJevModel({ providerID: "p", id: "m", variant: "v" })).toEqual({ providerID: "p", id: "m", variant: "v" });
    expect(parseJevModel("bad")).toEqual({ providerID: "opencode", id: "jev-1.13-free" });
});

test("resume options are opt-in and validate positive finite thresholds", () => {
    expect(resolveOptions({}).resume).toBeUndefined();
    for (const resume of [false, null, [], "true", 1]) {
        expect(resolveOptions({ resume }).resume).toBeUndefined();
    }
    expect(resolveOptions({ resume: true }).resume).toEqual({ idleMinutes: 60, minContextTokens: 100000 });
    expect(resolveOptions({ resume: {} }).resume).toEqual({ idleMinutes: 60, minContextTokens: 100000 });
    expect(resolveOptions({ resume: { idleMinutes: 0.5, minContextTokens: 42.5 } }).resume)
        .toEqual({ idleMinutes: 0.5, minContextTokens: 42.5 });
    for (const value of [0, -1, Infinity, NaN, "5", null]) {
        expect(resolveOptions({ resume: { idleMinutes: value, minContextTokens: value } }).resume)
            .toEqual({ idleMinutes: 60, minContextTokens: 100000 });
    }
});

test("project primer is opt-in and validates thresholds", () => {
    for (const projectPrimer of [undefined, false, null, [], "true", 1]) {
        expect(resolveOptions({ projectPrimer }).projectPrimer).toBeUndefined();
    }
    expect(resolveOptions({ projectPrimer: true }).projectPrimer).toEqual({ budget: 4000, maxAgeHours: 72 });
    expect(resolveOptions({ projectPrimer: { budget: 500, maxAgeHours: 1.5 } }).projectPrimer)
        .toEqual({ budget: 500, maxAgeHours: 1.5 });
    for (const value of [0, -1, Infinity, NaN, "5", null]) {
        expect(resolveOptions({ projectPrimer: { budget: value, maxAgeHours: value } }).projectPrimer)
            .toEqual({ budget: 4000, maxAgeHours: 72 });
    }
});
