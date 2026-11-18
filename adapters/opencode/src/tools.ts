import { MementoError, type MementoClient, type SearchScope } from "./client";
import { judgeRelevance, type DecisionsFn, type JevModel } from "./jev";

export function createMemorySearchTool(deps: {
    client: Pick<MementoClient, "search">;
    decide?: DecisionsFn;
    jevModel: JevModel;
    harness: string;
    project: string;
    jevTimeoutMs?: number;
}) {
    return {
        name: "memory_search",
        description: "Search memento's stored memory of this session and earlier sessions.",
        input: {
            type: "object",
            properties: {
                query: { type: "string", description: "What to recall from stored memory." },
                scope: { type: "string", enum: ["session", "lineage", "project", "global"], description: "Which sessions to search." },
            },
            required: ["query"],
            additionalProperties: false,
        },
        async execute(input: unknown, context: { sessionID: string; signal: AbortSignal }): Promise<{ content: string }> {
            if (!input || typeof input !== "object" || Array.isArray(input)) {
                return { content: "memory_search: input must be an object." };
            }
            const value = input as Record<string, unknown>;
            if (typeof value.query !== "string" || !value.query.trim()) {
                return { content: "memory_search: query must be a non-empty string." };
            }
            if (value.scope !== undefined && !["session", "lineage", "project", "global"].includes(value.scope as string)) {
                return { content: "memory_search: invalid scope." };
            }
            if (Object.keys(value).some((key) => key !== "query" && key !== "scope")) {
                return { content: "memory_search: unexpected input field." };
            }
            const query = value.query;
            const scope = value.scope as SearchScope | undefined;
            try {
                const result = await deps.client.search({
                    harness: deps.harness,
                    project: deps.project,
                    session: context.sessionID,
                    query,
                    ...(scope === undefined ? {} : { scope }),
                    k: 12,
                }, context.signal);
                let hits = result.hits;
                if (deps.decide) {
                    const decide = deps.decide;
                    const proseKinds = new Set(["user_request", "assistant_note", "decision", "constraint", "task"]);
                    let timer: ReturnType<typeof setTimeout> | undefined;
                    try {
                        const judgments = await Promise.race([
                            Promise.all(hits.map(async (hit) => {
                                if (!proseKinds.has(hit.kind)) {
                                    return true;
                                }
                                const probability = await judgeRelevance(decide, deps.jevModel, query, hit.text);
                                if (probability === undefined) {
                                    throw new Error("Jev relevance judgment failed");
                                }
                                return probability >= 0.5;
                            })),
                            new Promise<never>((_, reject) => {
                                timer = setTimeout(() => reject(new Error("Jev relevance judgment timed out")), deps.jevTimeoutMs ?? 3000);
                            }),
                        ]);
                        hits = hits.filter((_, index) => judgments[index]);
                    } catch {
                        hits = result.hits;
                    } finally {
                        if (timer !== undefined) {
                            clearTimeout(timer);
                        }
                    }
                }
                hits = [...hits].sort((a, b) => b.score - a.score).slice(0, 6);
                if (!hits.length) {
                    return { content: `No relevant memories found for "${query}".` };
                }
                const header = `Found ${hits.length} memories for "${query}" (scope: ${scope ?? "lineage"}).`;
                const content = hits.map((hit, index) => `\n\n[${index + 1}] ${hit.kind} | session ${hit.session} | ${hit.ts}${hit.path ? ` | ${hit.path}` : ""}\n${hit.text}`).join("");
                return { content: header + content };
            } catch (error) {
                if (error instanceof MementoError) {
                    return { content: `memento daemon unavailable: ${error.message}` };
                }
                throw error;
            }
        },
    };
}
