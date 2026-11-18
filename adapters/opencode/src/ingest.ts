import type { MementoClient } from "./client";
import { classifyKind, type DecisionsFn, type JevModel } from "./jev";
import { mapMessage, selectNewMessages } from "./transcript";

type Storage = {
    get(key: string): Promise<unknown>;
    set(key: string, value: string | boolean): Promise<unknown>;
};

export function createIngestor(deps: {
    client: Pick<MementoClient, "putSession" | "ingest">;
    storage: Storage;
    loadContext(sessionID: string): Promise<unknown[]>;
    decide?: DecisionsFn;
    jevModel: JevModel;
    harness: string;
    project: string;
    log(message: string, error?: unknown): void;
}) {
    const chains = new Map<string, Promise<void>>();
    const registered = new Set<string>();
    const classifications = new Map<string, Map<string, { text: string; kind: string | null }>>();

    async function markRegistered(sessionID: string): Promise<void> {
        await deps.storage.set(`memento/registered/${sessionID}`, true);
        registered.add(sessionID);
    }

    async function ingestSession(sessionID: string): Promise<void> {
        const cursorValue = await deps.storage.get(`memento/ingest/${sessionID}`);
        const cursor = typeof cursorValue === "string" ? cursorValue : undefined;
        const context = await deps.loadContext(sessionID);
        const selection = selectNewMessages(context, cursor);
        const items = selection.messages.flatMap(mapMessage);
        if (deps.decide) {
            const decide = deps.decide;
            const assistantItems = items.filter((item) => item.role === "assistant");
            let sessionClassifications = classifications.get(sessionID);
            if (!sessionClassifications) {
                sessionClassifications = new Map();
                classifications.set(sessionID, sessionClassifications);
            }
            const cache = sessionClassifications;
            let index = 0;
            await Promise.all(Array.from({ length: Math.min(4, assistantItems.length) }, async () => {
                while (index < assistantItems.length) {
                    const item = assistantItems[index++];
                    const cached = cache.get(item.source_id);
                    if (cached?.text === item.text) {
                        item.kind_hint = cached.kind;
                    } else {
                        item.kind_hint = await classifyKind(decide, deps.jevModel, item.text);
                        cache.set(item.source_id, { text: item.text, kind: item.kind_hint });
                    }
                }
            }));
        }
        if (!registered.has(sessionID)) {
            if (await deps.storage.get(`memento/registered/${sessionID}`) !== true) {
                await deps.client.putSession({ harness: deps.harness, project: deps.project, session: sessionID });
                await markRegistered(sessionID);
            } else {
                registered.add(sessionID);
            }
        }
        if (items.length) {
            await deps.client.ingest({ harness: deps.harness, project: deps.project, session: sessionID, items });
        }
        if (selection.nextCursor !== undefined) {
            await deps.storage.set(`memento/ingest/${sessionID}`, selection.nextCursor);
        }
    }

    function flush(sessionID: string): Promise<void> {
        const previous = chains.get(sessionID) ?? Promise.resolve();
        const result = previous.then(() => ingestSession(sessionID));
        const healthy = result.catch(() => {});
        chains.set(sessionID, healthy);
        void healthy.then(() => {
            if (chains.get(sessionID) === healthy) {
                chains.delete(sessionID);
            }
        });
        return result;
    }

    function handleExecutionSucceeded(sessionID: string): Promise<void> {
        return flush(sessionID).catch((error: unknown) => {
            deps.log(`Failed to ingest session ${sessionID}`, error);
        });
    }

    async function idle(): Promise<void> {
        while (chains.size) {
            await Promise.all(chains.values());
        }
    }

    return { handleExecutionSucceeded, flush, idle, markRegistered };
}
