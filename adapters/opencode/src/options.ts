import { parseJevModel } from "./jev";

export function resolveOptions(raw: Record<string, unknown>) {
    const url = typeof raw.url === "string" && raw.url.trim()
        ? raw.url
        : typeof process.env.MEMENTO_URL === "string" && process.env.MEMENTO_URL.trim()
            ? process.env.MEMENTO_URL
            : "http://127.0.0.1:7377";
    const token = raw.token === undefined ? process.env.MEMENTO_TOKEN : typeof raw.token === "string" && raw.token ? raw.token : undefined;
    const tailUserTurns = typeof raw.tailUserTurns === "number" && Number.isInteger(raw.tailUserTurns) && raw.tailUserTurns >= 1
        ? raw.tailUserTurns : 3;
    const digestBudget = typeof raw.digestBudget === "number" && Number.isInteger(raw.digestBudget) && raw.digestBudget > 0
        ? raw.digestBudget : 12000;
    const tailCharBudget = typeof raw.tailCharBudget === "number" && Number.isInteger(raw.tailCharBudget) && raw.tailCharBudget > 0
        ? raw.tailCharBudget : 40000;
    const jevCooldownMs = typeof raw.jevCooldownMs === "number" && Number.isSafeInteger(raw.jevCooldownMs) && raw.jevCooldownMs >= 0
        ? raw.jevCooldownMs : 600000;
    return {
        url,
        token,
        tailUserTurns,
        tailCharBudget,
        digestBudget,
        enabled: typeof raw.enabled === "boolean" ? raw.enabled : true,
        reconcile: typeof raw.reconcile === "boolean" ? raw.reconcile : true,
        jev: typeof raw.jev === "boolean" ? raw.jev : true,
        jevCooldownMs,
        jevModel: parseJevModel(raw.jevModel),
        harness: typeof raw.harness === "string" && raw.harness.trim() ? raw.harness : "opencode",
    };
}
