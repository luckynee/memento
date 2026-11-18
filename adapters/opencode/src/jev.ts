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
