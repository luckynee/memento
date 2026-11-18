import { expect, test } from "bun:test";
import { resolveOptions } from "../src/options";
import { parseJevModel } from "../src/jev";

test("validates adapter options and model references", () => {
    const defaults = resolveOptions({ tailUserTurns: 0, tailCharBudget: -1, digestBudget: 1.5, enabled: "no", harness: "", jevCooldownMs: -1 });
    expect(defaults).toMatchObject({ tailUserTurns: 3, tailCharBudget: 40000, digestBudget: 12000, enabled: true, jev: true, jevCooldownMs: 600000, harness: "opencode" });
    expect(resolveOptions({ enabled: false, harness: "custom", tailCharBudget: 99, jevCooldownMs: 0 })).toMatchObject({ enabled: false, harness: "custom", tailCharBudget: 99, jevCooldownMs: 0 });
    expect(parseJevModel("provider/model")).toEqual({ providerID: "provider", id: "model" });
    expect(parseJevModel({ providerID: "p", id: "m", variant: "v" })).toEqual({ providerID: "p", id: "m", variant: "v" });
    expect(parseJevModel("bad")).toEqual({ providerID: "opencode", id: "jev-1.13-free" });
});
