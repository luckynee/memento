export type JevModel = { providerID: string; id: string; variant?: string };
export const DEFAULT_JEV_MODEL: JevModel = { providerID: "opencode", id: "jev-1.13-free" };

const scaffoldStripTags = new Set([
    "system-reminder", "skill_content", "environment_context", "user_instructions",
    "command-message", "command-name", "local-command-stdout", "local-command-stderr",
    "local-command-caveat", "subagent", "memento-digest", "memento-memory", "memento-continuation",
]);

export function stripScaffolding(text: string): string {
    let output = "";
    let position = 0;
    const tags: { start: number; end: number; name: string; closing: boolean }[] = [];
    while (true) {
        const start = text.indexOf("<", position);
        if (start < 0) {
            break;
        }
        const match = /^<(\/?)([a-zA-Z0-9_-]+)(?=>|\s)/u.exec(text.slice(start));
        let end = start + (match?.[0].length ?? 1);
        let quote: string | undefined;
        if (match) {
            while (end < text.length) {
                const character = text[end++];
                if (quote === character) {
                    quote = undefined;
                } else if (!quote && (character === "'" || character === '"')) {
                    quote = character;
                } else if (!quote && character === ">") {
                    break;
                }
            }
        }
        if (!match || end > text.length || text[end - 1] !== ">" || quote) {
            position = start + 1;
            continue;
        }
        tags.push({ start, end, name: match[2].toLowerCase(), closing: match[1] === "/" });
        position = end;
    }
    const matchingEnds = new Map<number, number>();
    const openings = new Map<string, number[]>();
    for (const [index, tag] of tags.entries()) {
        if (!scaffoldStripTags.has(tag.name)) {
            continue;
        }
        if (tag.closing) {
            const opening = openings.get(tag.name)?.pop();
            if (opening !== undefined) {
                matchingEnds.set(opening, tag.end);
            }
        } else {
            const sameName = openings.get(tag.name) ?? [];
            sameName.push(index);
            openings.set(tag.name, sameName);
        }
    }
    position = 0;
    for (const [index, tag] of tags.entries()) {
        if (tag.start < position) {
            continue;
        }
        output += text.slice(position, tag.start);
        const end = matchingEnds.get(index);
        if (end !== undefined) {
            position = end;
        } else {
            if (tag.name !== "command-args") {
                output += text.slice(tag.start, tag.end);
            }
            position = tag.end;
        }
    }
    output += text.slice(position);
    return output === text ? output : output.trim();
}

type Question =
    | { type: "predicate"; name: string; instructions: string }
    | { type: "choice"; name: string; instructions: string; choices: { value: string | boolean; description?: string }[] };

type Answer =
    | { type: "predicate"; name: string; probability: number }
    | { type: "choice"; name: string; choice: string | boolean; confidence?: number }
    | { type: "score"; name: string }
    | { type: "error"; name: string; reason: string; message?: string };

export type DecisionsFn = (input: {
    model: JevModel;
    input: { type: "text"; text: string }[];
    questions: Question[];
}) => Promise<{ answers: ReadonlyArray<Answer> }>;

export async function forEachJevItem<T>(items: readonly T[], process: (item: T, index: number) => Promise<void>): Promise<void> {
    let index = 0;
    await Promise.all(Array.from({ length: Math.min(4, items.length) }, async () => {
        while (index < items.length) {
            const currentIndex = index++;
            await process(items[currentIndex], currentIndex);
        }
    }));
}

type JevErrorDetails = {
    reason?: string;
    status?: number;
    retryAfterMs?: number;
    errorClass?: string;
    message?: string;
};

function jevErrorDetails(error: unknown): JevErrorDetails {
    const details: JevErrorDetails = {};
    const pending: unknown[] = [error];
    const seen = new Set<unknown>();
    let fallbackClass: string | undefined;
    while (pending.length && seen.size < 32) {
        const value = pending.shift();
        if ((typeof value !== "object" && typeof value !== "function") || value === null || seen.has(value)) {
            continue;
        }
        seen.add(value);
        try {
            const record = value as Record<string, unknown>;
            if (details.reason === undefined && typeof record.reason === "string" && record.reason.trim()) {
                details.reason = record.reason;
            }
            if (details.status === undefined && typeof record.status === "number" && Number.isFinite(record.status)) {
                details.status = record.status;
            }
            if (details.retryAfterMs === undefined && typeof record.retryAfterMs === "number"
                && Number.isFinite(record.retryAfterMs) && record.retryAfterMs >= 0) {
                details.retryAfterMs = record.retryAfterMs;
            }
            if (details.errorClass === undefined && typeof record._tag === "string" && record._tag.trim()) {
                details.errorClass = record._tag;
            }
            if (value instanceof Error && !fallbackClass && value.name !== "Error") {
                fallbackClass = value.name;
            }
            pending.push(record.cause, record.data, record.error);
            if (typeof record.message === "string") {
                details.message ??= record.message;
                const message = record.message.trimStart();
                if (message.startsWith("{")) {
                    try {
                        pending.push(JSON.parse(message));
                    } catch {}
                }
            }
        } catch {}
    }
    details.errorClass ??= fallbackClass;
    return details;
}

function safeJevMessage(message: string): string {
    return message
        .replace(/\bBearer\s+\S+/gi, "Bearer [redacted]")
        .replace(/\b(api[_-]?key|token|secret|password|authorization)\b\s*[:=]\s*["']?[^\s,"']+/gi, "$1=[redacted]")
        .replace(/\b(sk|pk|key|token)[-_][a-z0-9_-]{8,}\b/gi, "[redacted]")
        .replace(/[a-z][a-z\d+.-]*:\/\/[^\s"'`<>{}[\]]*/gi, "")
        .replace(/\b(?:bearer|token|authorization|api[_-]?key|password|secret)\b/gi, "redacted")
        .replace(/[{}[\]"'`]/g, "")
        .replace(/[\r\n\t]+/g, " ")
        .replace(/ {2,}/g, " ")
        .trim();
}

function formatJevCooldown(delayMs: number): string {
    const seconds = Math.ceil(delayMs / 1000);
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor(seconds % 3600 / 60);
    const remainder = seconds % 60;
    return `${hours ? `${hours}h` : ""}${minutes ? `${minutes}m` : ""}${remainder || (!hours && !minutes) ? `${remainder}s` : ""}`;
}

export function createJevGate(deps: {
    decide: DecisionsFn;
    cooldownMs: number;
    log(message: string): void;
    now?: () => number;
    timeoutMs?: number;
    scheduleTimeout?: (callback: () => void, delay: number) => () => void;
}) {
    const now = deps.now ?? Date.now;
    const scheduleTimeout = deps.scheduleTimeout ?? ((callback, delay) => {
        const timer = setTimeout(callback, delay);
        return () => clearTimeout(timer);
    });
    let retryAt = 0;
    let generation = 0;

    const decide: DecisionsFn = async (input) => {
        if (now() < retryAt) {
            throw new Error("Jev cooldown active");
        }
        const callGeneration = generation;
        let cancelTimeout: (() => void) | undefined;
        try {
            const result = await Promise.race([
                deps.decide(input),
                new Promise<never>((_, reject) => {
                    cancelTimeout = scheduleTimeout(() => reject(new Error("Jev request timed out")), deps.timeoutMs ?? 3000);
                }),
            ]);
            const errors = result.answers.filter((answer) => answer.type === "error");
            if (input.questions.length > 0 && input.questions.every((question) => errors.some((answer) => answer.name === question.name))) {
                const error = errors[0];
                throw new Error(error.message ?? error.reason);
            }
            if (callGeneration === generation) {
                retryAt = 0;
            }
            return result;
        } catch (error) {
            if (callGeneration === generation) {
                const details = jevErrorDetails(error);
                const cooldownMs = details.retryAfterMs === undefined
                    ? deps.cooldownMs
                    : Math.max(deps.cooldownMs, Math.min(details.retryAfterMs, 24 * 60 * 60 * 1000));
                retryAt = now() + cooldownMs;
                generation++;
                const structured = [
                    details.reason ?? details.errorClass,
                    details.status === undefined ? undefined : `status=${details.status}`,
                    details.retryAfterMs === undefined ? undefined : `retry in ${formatJevCooldown(cooldownMs)}`,
                ].filter((part): part is string => part !== undefined).join(" ");
                let fallback = details.message;
                if (fallback === undefined) {
                    try {
                        fallback = String(error);
                    } catch {
                        fallback = "unknown error";
                    }
                }
                const message = safeJevMessage(structured || fallback) || "unknown error";
                deps.log(Array.from(`Jev cooldown started: ${message}`).slice(0, 300).join(""));
            }
            throw error;
        } finally {
            cancelTimeout?.();
        }
    };
    return decide;
}

export function parseJevModel(option: unknown): JevModel {
    if (typeof option === "string") {
        const parts = option.split("/");
        if (parts.length === 2 && parts[0] && parts[1]) {
            return { providerID: parts[0], id: parts[1] };
        }
    }
    if (option && typeof option === "object" && "providerID" in option && "id" in option
        && typeof option.providerID === "string" && option.providerID
        && typeof option.id === "string" && option.id
        && (!("variant" in option) || option.variant === undefined || typeof option.variant === "string")) {
        return {
            providerID: option.providerID,
            id: option.id,
            ...("variant" in option && typeof option.variant === "string" ? { variant: option.variant } : {}),
        };
    }
    return { ...DEFAULT_JEV_MODEL };
}

export async function classifyKind(decide: DecisionsFn, model: JevModel, text: string, role: "user" | "assistant" = "assistant"): Promise<"decision" | "constraint" | "task" | null> {
    const classificationText = stripScaffolding(text);
    if (!classificationText.trim()) {
        return null;
    }
    try {
        const result = await decide({
            model,
            input: [{ type: "text", text: classificationText.slice(0, 4000) }],
            questions: [{
                type: "choice",
                name: "kind",
                instructions: `Classify this ${role} message from a coding session.`,
                choices: [
                    { value: "decision", description: "A design decision" },
                    { value: "constraint", description: "A requirement or constraint" },
                    ...(role === "assistant" ? [{ value: "task", description: "A task or next step" }] : []),
                    role === "user"
                        ? { value: "request", description: "Other user requests" }
                        : { value: "note", description: "Other information" },
                ],
            }],
        });
        const answer = result.answers.find((entry) => entry.name === "kind" && entry.type === "choice");
        if (answer?.type === "choice"
            && (answer.confidence === undefined
                ? role === "assistant"
                : typeof answer.confidence === "number" && Number.isFinite(answer.confidence) && answer.confidence >= 0.6)
            && (answer.choice === "decision" || answer.choice === "constraint" || (role === "assistant" && answer.choice === "task"))) {
            return answer.choice;
        }
    } catch {
        return null;
    }
    return null;
}

export async function judgeRelevance(decide: DecisionsFn, model: JevModel, query: string, text: string): Promise<number | undefined> {
    try {
        const result = await decide({
            model,
            input: [{ type: "text", text: `Query: ${query}\n\nMemory:\n${text.slice(0, 4000)}` }],
            questions: [{ type: "predicate", name: "relevant", instructions: "Is this memory relevant to the query?" }],
        });
        const answer = result.answers.find((entry) => entry.name === "relevant" && entry.type === "predicate");
        return answer?.type === "predicate" && Number.isFinite(answer.probability) ? answer.probability : undefined;
    } catch {
        return undefined;
    }
}
