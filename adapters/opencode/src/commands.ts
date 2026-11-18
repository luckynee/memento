import type { MementoClient } from "./client";

export function createContinueCommand(deps: {
    client: Pick<MementoClient, "putSession" | "digest">;
    putInstruction(sessionID: string, key: string, value: string): Promise<unknown>;
    markRegistered(sessionID: string): Promise<void>;
    harness: string;
    project: string;
    digestBudget: number;
}) {
    return {
        name: "memento-continue",
        description: "Continue from an earlier session's stored memory.",
        async execute({ sessionID, prompt }: { sessionID: string; prompt: { text: string } }): Promise<void> {
            const parent = prompt.text.trim().split(/\s+/)[0];
            if (!parent) {
                throw new Error("memento-continue requires a parent session id");
            }
            if (parent === sessionID) {
                throw new Error("memento-continue cannot use the current session as its parent");
            }
            await deps.client.putSession({ harness: deps.harness, project: deps.project, session: sessionID, parent });
            await deps.markRegistered(sessionID);
            const digest = await deps.client.digest({ harness: deps.harness, project: deps.project, session: sessionID, budget: deps.digestBudget });
            await deps.putInstruction(sessionID, "memento-continuation", digest.text);
        },
    };
}
