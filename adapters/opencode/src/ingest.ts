import { createHash } from "node:crypto";
import { recordActivity, type ActivitySink } from "./activity-log";
import { MementoError, type MementoClient } from "./client";
import { frameContinuation } from "./commands";
import { classifyKind, forEachJevItem, type DecisionsFn, type JevModel } from "./jev";
import { mapMessage, selectNewMessages, type IngestItem } from "./transcript";

type Storage = {
    get(key: string): Promise<unknown>;
    set(key: string, value: string | boolean): Promise<unknown>;
    remove?(key: string): Promise<unknown>;
};

export const INGEST_BATCH_MAX_ITEMS = 64;
export const INGEST_BATCH_MAX_BYTES = 2 * 1024 * 1024;

export function batchMessages(messages: unknown[], maxItems = INGEST_BATCH_MAX_ITEMS, maxBytes = INGEST_BATCH_MAX_BYTES): { items: IngestItem[]; cursor?: string }[] {
    const batches: { items: IngestItem[]; cursor?: string }[] = [];
    let current: { items: IngestItem[]; cursor?: string; bytes: number } | undefined;
    for (const message of messages) {
        const items = mapMessage(message);
        const bytes = items.reduce((sum, item) => sum + JSON.stringify(item).length, 0);
        if (current && current.items.length && (current.items.length + items.length > maxItems || current.bytes + bytes > maxBytes)) {
            batches.push({ items: current.items, cursor: current.cursor });
            current = undefined;
        }
        current ??= { items: [], bytes: 0 };
        current.items.push(...items);
        current.bytes += bytes;
        const id = message && typeof message === "object" ? (message as { id?: unknown }).id : undefined;
        if (typeof id === "string") {
            current.cursor = id;
        }
    }
    if (current) {
        batches.push({ items: current.items, cursor: current.cursor });
    }
    return batches;
}

export function createIngestor(deps: {
    client: Pick<MementoClient, "putSession" | "ingest" | "claimContinuation">;
    storage: Storage;
    loadContext(sessionID: string): Promise<unknown[]>;
    loadSession?(sessionID: string): Promise<{ fork?: { sessionID: string }; time: { created: number | Date | string } }>;
    putInstruction(sessionID: string, key: string, value: string): Promise<unknown>;
    decide?: DecisionsFn;
    jevModel: JevModel;
    harness: string;
    project: string;
    activity?: ActivitySink;
    log(message: string, error?: unknown): void;
}) {
    const chains = new Map<string, Promise<void>>();
    const retries = new Map<string, Promise<void>>();
    const sessions = new Map<string, {
        registered: boolean;
        retryClaim: boolean;
        classifications: Map<string, { hash: string; kind: string | null }>;
        lastIngestHash?: string;
    }>();

    function sessionState(sessionID: string) {
        const state = sessions.get(sessionID) ?? { registered: false, retryClaim: false, classifications: new Map<string, { hash: string; kind: string | null }>() };
        sessions.delete(sessionID);
        sessions.set(sessionID, state);
        if (sessions.size > 64) {
            sessions.delete(sessions.keys().next().value!);
        }
        return state;
    }

    async function markRegistered(sessionID: string): Promise<void> {
        await deps.storage.set(`memento/registered/${sessionID}`, true);
        sessionState(sessionID).registered = true;
    }

    async function registerSession(sessionID: string, waitForStartup = true): Promise<string | undefined> {
        const state = sessionState(sessionID);
        if (state.registered && !state.retryClaim) {
            return;
        }
        const persistedRegistration = await deps.storage.get(`memento/registered/${sessionID}`);
        if (!state.retryClaim && persistedRegistration === true) {
            state.registered = true;
            return;
        }
        if (persistedRegistration === "claim-pending") {
            state.registered = true;
            state.retryClaim = true;
        }
        let fork: { sessionID: string; created: number } | undefined;
        if (!state.registered && deps.loadSession) {
            try {
                const session = await deps.loadSession(sessionID);
                if (session.fork) {
                    const created = new Date(session.time.created).getTime();
                    if (typeof session.fork.sessionID !== "string" || !session.fork.sessionID
                        || !Number.isFinite(created)) {
                        throw new Error("Fork parent or creation time is missing");
                    }
                    fork = { sessionID: session.fork.sessionID, created };
                    await deps.storage.set(`memento/fork/${sessionID}`, JSON.stringify(fork));
                }
            } catch (error) {
                deps.log(`Failed to look up fork for session ${sessionID}`, error);
            }
        }
        const claimKey = `memento/claimed/${sessionID}`;
        let claimedParent = false;
        let continuation: string | undefined;
        if (await deps.storage.get(claimKey) !== true) {
            try {
                const claim = await deps.client.claimContinuation({ project: deps.project, harness: deps.harness, session: sessionID }, waitForStartup);
                if (!claim || typeof claim.claimed !== "boolean") {
                    throw new Error("Invalid continuation claim response");
                }
                if (claim.claimed) {
                    if (typeof claim.parent_harness !== "string" || typeof claim.parent !== "string" || typeof claim.digest !== "string") {
                        throw new Error("Claimed continuation is missing its parent or digest");
                    }
                }
                if (claim.claimed) {
                    claimedParent = true;
                    await deps.storage.set(`memento/parent/${sessionID}`, JSON.stringify({ harness: claim.parent_harness, session: claim.parent }));
                    const framed = frameContinuation(claim.parent_harness!, claim.parent!, claim.digest!);
                    await deps.putInstruction(sessionID, "memento-continuation", framed);
                    continuation = framed;
                }
                await deps.storage.set(claimKey, true);
                state.retryClaim = false;
                recordActivity(deps.activity, { level: "info", event: "continuation", session: sessionID, project: deps.project, detail: { claimed: claim.claimed } });
            } catch (error) {
                if (await deps.storage.get(claimKey) !== true) {
                    state.retryClaim = true;
                }
                deps.log(`Failed to claim continuation for session ${sessionID}`, error);
            }
        }
        if (!state.registered) {
            const hasClaimedParent = claimedParent || typeof await deps.storage.get(`memento/parent/${sessionID}`) === "string";
            const identity = {
                harness: deps.harness,
                project: deps.project,
                session: sessionID,
            };
            try {
                await deps.client.putSession({
                    ...identity,
                    ...(fork && !hasClaimedParent ? { parent_harness: deps.harness, parent: fork.sessionID, parent_cutoff: new Date(fork.created).toISOString(), require_parent: true } : {}),
                }, waitForStartup);
            } catch (error) {
                if (!fork || hasClaimedParent || !(error instanceof MementoError) || error.code !== "parent_unavailable") {
                    throw error;
                }
                if (!deps.storage.remove) {
                    throw new Error(`Cannot clear fork state for session ${sessionID}`);
                }
                await deps.storage.remove(`memento/fork/${sessionID}`);
                await deps.client.putSession(identity, waitForStartup);
            }
            recordActivity(deps.activity, { level: "info", event: "register", session: sessionID, project: deps.project });
            if (state.retryClaim) {
                await deps.storage.set(`memento/registered/${sessionID}`, "claim-pending");
                state.registered = true;
            } else {
                await markRegistered(sessionID);
            }
        } else if (!state.retryClaim && persistedRegistration === "claim-pending") {
            await markRegistered(sessionID);
        }
        return continuation;
    }

    async function ingestSession(sessionID: string, waitForStartup = true): Promise<void> {
        await registerSession(sessionID, waitForStartup);
        const cursorValue = await deps.storage.get(`memento/ingest/${sessionID}`);
        const cursor = typeof cursorValue === "string" ? cursorValue : undefined;
        const context = await deps.loadContext(sessionID);
        const forkValue = await deps.storage.get(`memento/fork/${sessionID}`);
        let forkCreated: number | undefined;
        if (typeof forkValue === "string") {
            const fork = JSON.parse(forkValue) as { created?: unknown };
            if (typeof fork.created === "number" && Number.isFinite(fork.created)) {
                forkCreated = fork.created;
            }
        }
        const forkContext = forkCreated === undefined ? context : context.filter((value) => {
            if (!value || typeof value !== "object" || !("time" in value)) {
                return true;
            }
            const time = value.time;
            if (!time || typeof time !== "object" || !("created" in time) || typeof time.created !== "number") {
                return true;
            }
            return time.created >= forkCreated;
        });
        const selection = selectNewMessages(forkContext, cursor);
        const batches = batchMessages(selection.messages);
        const totalItems = batches.reduce((sum, batch) => sum + batch.items.length, 0);
        if (totalItems) {
            const hash = createHash("sha256").update(JSON.stringify(batches.flatMap((batch) => batch.items))).digest("hex");
            if (selection.nextCursor === cursor && sessionState(sessionID).lastIngestHash === hash) {
                return;
            }
            for (const [index, batch] of batches.entries()) {
                const items = batch.items;
                if (deps.decide) {
                    const decide = deps.decide;
                    const textItems = items.filter((item) => item.role === "assistant" || item.role === "user");
                    const cache = sessionState(sessionID).classifications;
                    await forEachJevItem(textItems, async (item) => {
                        const itemHash = createHash("sha256").update(item.role).update("\0").update(item.text).digest("hex");
                        const cached = cache.get(item.source_id);
                        if (cached?.hash === itemHash) {
                            item.kind_hint = cached.kind;
                        } else {
                            item.kind_hint = await classifyKind(decide, deps.jevModel, item.text, item.role as "user" | "assistant");
                        }
                        cache.delete(item.source_id);
                        cache.set(item.source_id, { hash: itemHash, kind: item.kind_hint });
                        if (cache.size > 500) {
                            cache.delete(cache.keys().next().value!);
                        }
                    });
                }
                if (items.length) {
                    const started = Date.now();
                    const result = await deps.client.ingest({ harness: deps.harness, project: deps.project, session: sessionID, items }, waitForStartup);
                    recordActivity(deps.activity, { level: "info", event: "ingest", session: sessionID, project: deps.project, detail: { items: items.length, segments: result.segments, skipped: result.skipped, durationMs: Date.now() - started, ...(batches.length > 1 ? { batch: index + 1, batches: batches.length } : {}) } });
                }
                if (index < batches.length - 1 && batch.cursor !== undefined) {
                    await deps.storage.set(`memento/ingest/${sessionID}`, batch.cursor);
                }
            }
            sessionState(sessionID).lastIngestHash = hash;
        }
        if (selection.nextCursor !== undefined) {
            await deps.storage.set(`memento/ingest/${sessionID}`, selection.nextCursor);
        }
    }

    function serialize<T>(sessionID: string, action: () => Promise<T>): Promise<T> {
        const previous = chains.get(sessionID) ?? Promise.resolve();
        const result = previous.then(action);
        const healthy = result.then(() => {}, () => {});
        chains.set(sessionID, healthy);
        void healthy.then(() => {
            if (chains.get(sessionID) === healthy) {
                chains.delete(sessionID);
            }
        });
        return result;
    }

    function ensureRegistered(sessionID: string): Promise<string | undefined> {
        return serialize(sessionID, () => registerSession(sessionID));
    }

    function flush(sessionID: string): Promise<void> {
        return serialize(sessionID, () => ingestSession(sessionID));
    }

    function handleExecutionSucceeded(sessionID: string): Promise<void> {
        return serialize(sessionID, () => ingestSession(sessionID, false)).catch((error: unknown) => {
            deps.log(`Failed to ingest session ${sessionID}`, error);
            if (!(error instanceof MementoError) || !error.connectionFailure || !error.startup || retries.has(sessionID)) {
                return;
            }
            const retry = error.startup.then(async (ready) => {
                if (!ready || retries.get(sessionID) !== retry) {
                    return;
                }
                recordActivity(deps.activity, { level: "info", event: "ingest", session: sessionID, project: deps.project, detail: { count: 1, reason: "autostart_retry" } });
                try {
                    await serialize(sessionID, () => ingestSession(sessionID, false));
                } catch (retryError) {
                    deps.log(`Failed to retry ingest for session ${sessionID}`, retryError);
                }
            }).finally(() => {
                if (retries.get(sessionID) === retry) {
                    retries.delete(sessionID);
                }
            });
            retries.set(sessionID, retry);
        });
    }

    async function idle(): Promise<void> {
        while (chains.size) {
            await Promise.all(chains.values());
        }
    }

    async function forget(sessionID: string): Promise<void> {
        retries.delete(sessionID);
        while (chains.has(sessionID)) {
            await chains.get(sessionID);
        }
        sessions.delete(sessionID);
    }

    return { handleExecutionSucceeded, flush, idle, forget, markRegistered, ensureRegistered };
}
