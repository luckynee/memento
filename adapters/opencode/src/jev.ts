export type JevModel = { providerID: string; id: string; variant?: string };
export const DEFAULT_JEV_MODEL: JevModel = { providerID: "opencode", id: "jev-1.13" };

type Question =
    | { type: "predicate"; name: string; instructions: string }
    | { type: "choice"; name: string; instructions: string; choices: { value: string | boolean; description?: string }[] };

type Answer =
    | { type: "predicate"; name: string; probability: number }
    | { type: "choice"; name: string; choice: string | boolean }
    | { type: "score"; name: string }
    | { type: "error"; name: string; reason: string; message?: string };

export type DecisionsFn = (input: {
    model: JevModel;
    input: { type: "text"; text: string }[];
    questions: Question[];
}) => Promise<{ answers: ReadonlyArray<Answer> }>;

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
                retryAt = now() + deps.cooldownMs;
                generation++;
                const message = error instanceof Error ? error.message : String(error);
                const safeMessage = message
                    .replace(/\bBearer\s+\S+/gi, "Bearer [redacted]")
                    .replace(/\b(api[_-]?key|token|secret|password|authorization)\b\s*[:=]\s*["']?[^\s,"']+/gi, "$1=[redacted]")
                    .replace(/\b(sk|pk|key|token)[-_][a-z0-9_-]{8,}\b/gi, "[redacted]");
                deps.log(`Jev cooldown started: ${safeMessage}`);
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

export async function classifyKind(decide: DecisionsFn, model: JevModel, text: string): Promise<"decision" | "constraint" | "task" | null> {
    try {
        const result = await decide({
            model,
            input: [{ type: "text", text: text.slice(0, 4000) }],
            questions: [{
                type: "choice",
                name: "kind",
                instructions: "Classify this assistant message from a coding session.",
                choices: [
                    { value: "decision", description: "A design decision" },
                    { value: "constraint", description: "A requirement or constraint" },
                    { value: "task", description: "A task or next step" },
                    { value: "note", description: "Other information" },
                ],
            }],
        });
        const answer = result.answers.find((entry) => entry.name === "kind" && entry.type === "choice");
        if (answer?.type === "choice" && (answer.choice === "decision" || answer.choice === "constraint" || answer.choice === "task")) {
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
