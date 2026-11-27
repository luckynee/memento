import { parseJevModel } from "./jev";

export type SubagentMemoryMode = "off" | "digest" | "filtered";

function isSubagentMemoryMode(value: unknown): value is SubagentMemoryMode {
    return value === "off" || value === "digest" || value === "filtered";
}

export function resolveOptions(raw: Record<string, unknown>) {
    const resumeInput = raw.resume === true ? {} : raw.resume && typeof raw.resume === "object" && !Array.isArray(raw.resume)
        ? raw.resume as Record<string, unknown> : undefined;
    const positiveNumber = (value: unknown, fallback: number) =>
        typeof value === "number" && Number.isFinite(value) && value > 0 ? value : fallback;
    const resume = resumeInput ? {
        idleMinutes: positiveNumber(resumeInput.idleMinutes, 60),
        minContextTokens: positiveNumber(resumeInput.minContextTokens, 100000),
    } : undefined;
    const primerInput = raw.projectPrimer === true ? {} : raw.projectPrimer && typeof raw.projectPrimer === "object" && !Array.isArray(raw.projectPrimer)
        ? raw.projectPrimer as Record<string, unknown> : undefined;
    const projectPrimer = primerInput ? {
        budget: typeof primerInput.budget === "number" && Number.isSafeInteger(primerInput.budget) && primerInput.budget > 0
            ? primerInput.budget : 4000,
        maxAgeHours: positiveNumber(primerInput.maxAgeHours, 72),
    } : undefined;
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
    const subagentDigestBudget = typeof raw.subagentDigestBudget === "number" && Number.isInteger(raw.subagentDigestBudget) && raw.subagentDigestBudget > 0
        ? raw.subagentDigestBudget : 12000;
    const tailCharBudget = typeof raw.tailCharBudget === "number" && Number.isInteger(raw.tailCharBudget) && raw.tailCharBudget > 0
        ? raw.tailCharBudget : 40000;
    const jevCooldownMs = typeof raw.jevCooldownMs === "number" && Number.isSafeInteger(raw.jevCooldownMs) && raw.jevCooldownMs >= 0
        ? raw.jevCooldownMs : 600000;
    const subagentMemoryAgents: Record<string, SubagentMemoryMode> = Object.fromEntries(
        raw.subagentMemoryAgents && typeof raw.subagentMemoryAgents === "object" && !Array.isArray(raw.subagentMemoryAgents)
            ? Object.entries(raw.subagentMemoryAgents).filter((entry): entry is [string, SubagentMemoryMode] => isSubagentMemoryMode(entry[1]))
            : [],
    );
    return {
        url,
        token,
        autostart: raw.autostart === true,
        mementoBin: typeof raw.mementoBin === "string" ? raw.mementoBin : undefined,
        tailUserTurns,
        tailCharBudget,
        digestBudget,
        compaction: raw.compaction === "memento" ? "memento" as const : "native" as const,
        resume,
        projectPrimer,
        subagentMemory: isSubagentMemoryMode(raw.subagentMemory) ? raw.subagentMemory : "digest" as const,
        subagentMemoryAgents,
        subagentDigestBudget,
        enabled: typeof raw.enabled === "boolean" ? raw.enabled : true,
        activityLog: raw.activityLog === false ? false : typeof raw.activityLog === "string" && raw.activityLog.trim() ? raw.activityLog : true,
        reconcile: typeof raw.reconcile === "boolean" ? raw.reconcile : true,
        jev: typeof raw.jev === "boolean" ? raw.jev : true,
        jevCooldownMs,
        jevModel: parseJevModel(raw.jevModel),
        harness: typeof raw.harness === "string" && raw.harness.trim() ? raw.harness : "opencode",
    };
}
