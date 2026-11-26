import { recordActivity, type ActivitySink } from "./activity-log";
import type { MementoClient } from "./client";
import { buildDigestText, cloneMessage, dedupeCompactionSummary, isUserTurn, partSize, renderCurrentTurn, renderRecentTail, rewriteForSwitch, sameModel, type ConversationMessage, type ModelRef, type SwitchSnapshot, type SwitchState, type SystemText } from "./switch";

const SNAPSHOT_CACHE_TTL_MS = 300000;

type Storage = {
    get(key: string): Promise<unknown>;
    set(key: string, value: SwitchState): Promise<unknown>;
};

export function createSwitchController(deps: {
    client: Pick<MementoClient, "digest">;
    storage: Storage;
    flush(sessionID: string): Promise<void>;
    loadContext(sessionID: string): Promise<unknown[]>;
    harness: string;
    project: string;
    activity?: ActivitySink;
    digestBudget: number;
    compactionMode?: "native" | "memento";
    tailUserTurns: number;
    tailCharBudget: number;
    now?(): number;
    log(message: string, error?: unknown): void;
}) {
    const states = new Map<string, SwitchState>();
    const chains = new Map<string, Promise<void>>();

    function cache(sessionID: string, state: SwitchState): void {
        states.delete(sessionID);
        states.set(sessionID, state);
        if (states.size > 64) {
            states.delete(states.keys().next().value!);
        }
    }

    async function load(sessionID: string): Promise<SwitchState> {
        const cached = states.get(sessionID);
        if (cached) {
            cache(sessionID, cached);
            return cached;
        }
        const stored = await deps.storage.get(`memento/switch/${sessionID}`);
        const value = stored && typeof stored === "object" ? stored as Partial<SwitchState> : {};
        const state = { ...value, snapshots: value.snapshots ?? {} };
        cache(sessionID, state);
        return state;
    }

    async function persist(sessionID: string, state: SwitchState): Promise<void> {
        await deps.storage.set(`memento/switch/${sessionID}`, state);
        cache(sessionID, state);
    }

    function serialize(sessionID: string, action: () => Promise<void>): Promise<void> {
        const current = (chains.get(sessionID) ?? Promise.resolve()).then(action).catch((error: unknown) => {
            deps.log(`Failed to process model switch for session ${sessionID}`, error);
        });
        chains.set(sessionID, current);
        void current.then(() => {
            if (chains.get(sessionID) === current) {
                chains.delete(sessionID);
            }
        });
        return current;
    }

    function onModelSelected(sessionID: string, model: ModelRef, previous?: ModelRef): Promise<void> {
        return serialize(sessionID, async () => {
            const state = await load(sessionID);
            if (!state.origin) {
                const messages = await deps.loadContext(sessionID);
                const origin = historyModel(messages) ?? previous ?? model;
                await persist(sessionID, { ...state, origin: { providerID: origin.providerID, id: origin.id } });
            }
        });
    }

    function historyModel(messages: unknown[]): ModelRef | undefined {
        for (let index = messages.length - 1; index >= 0; index--) {
            const message = messages[index];
            if (!message || typeof message !== "object" || !("type" in message) || message.type !== "assistant" || !("model" in message)) {
                continue;
            }
            const model = message.model;
            if (model && typeof model === "object" && "providerID" in model && typeof model.providerID === "string" && "id" in model && typeof model.id === "string") {
                return { providerID: model.providerID, id: model.id };
            }
        }
        return undefined;
    }

    function snapshotIsStale(snapshot: SwitchSnapshot, messages: ConversationMessage[], now: number): boolean {
        if (snapshot.compacted || typeof snapshot.lastMessageID !== "string" || typeof snapshot.lastRequestAt !== "number" || !Number.isFinite(snapshot.lastRequestAt)) {
            return true;
        }
        if (now - snapshot.lastRequestAt > SNAPSHOT_CACHE_TTL_MS) {
            return true;
        }
        const lastMessageIndex = messages.findIndex((message) => message.id === snapshot.lastMessageID);
        if (lastMessageIndex < 0) {
            return true;
        }
        const addedSize = messages.slice(lastMessageIndex + 1).reduce(
            (sum, message) => sum + message.content.reduce<number>((total, part) => total + partSize(part), 0), 0,
        );
        return addedSize > deps.tailCharBudget;
    }

    function onCompaction(event: { sessionID: string; model: ModelRef; messages?: ReadonlyArray<ConversationMessage>; result?: { summary: string; metadata?: Record<string, unknown> } }): Promise<void> {
        return serialize(event.sessionID, async () => {
            const started = Date.now();
            const state = await load(event.sessionID);
            let context: unknown[];
            try {
                context = await deps.loadContext(event.sessionID);
            } catch (error) {
                throw new Error(`Failed to load context for compaction of session ${event.sessionID}`, { cause: error });
            }
            const previous = state.lastModel ?? historyModel(context) ?? state.origin;
            const sameModelCompaction = !previous || sameModel(event.model, previous);
            if (sameModelCompaction && deps.compactionMode !== "memento") {
                return;
            }
            await deps.flush(event.sessionID);
            const digest = await deps.client.digest({
                harness: deps.harness,
                project: deps.project,
                session: event.sessionID,
                budget: deps.digestBudget,
            });
            const model = { providerID: event.model.providerID, id: event.model.id };
            const modelKey = `${model.providerID}/${model.id}`;
            const messages = event.messages ?? [];
            const syntheticIDs = new Set(context.flatMap((message) => {
                if (!message || typeof message !== "object" || !("id" in message) || typeof message.id !== "string" || !("type" in message) || message.type !== "synthetic") {
                    return [];
                }
                return [message.id];
            }));
            let lastUserIndex = messages.length;
            for (let index = messages.length - 1; index >= 0; index--) {
                if (isUserTurn(messages[index], syntheticIDs)) {
                    lastUserIndex = index;
                    break;
                }
            }
            const tail = sameModelCompaction ? renderRecentTail(messages.slice(0, lastUserIndex), {
                tailUserTurns: deps.tailUserTurns,
                tailCharBudget: deps.tailCharBudget,
                syntheticIDs,
            }) : "";
            const currentTurn = sameModelCompaction ? renderCurrentTurn(messages.slice(lastUserIndex), deps.tailCharBudget, syntheticIDs) : "";
            const summary = [buildDigestText(digest.text), tail, currentTurn].filter(Boolean).join("\n\n");
            await persist(event.sessionID, {
                ...state,
                origin: state.origin ?? previous ?? model,
                lastModel: model,
                snapshots: { ...state.snapshots, [modelKey]: { cutMessageID: null, digest: digest.text, compacted: true } },
            });
            event.result = { summary, metadata: sameModelCompaction ? { source: "memento", mode: "same-model" } : { source: "memento" } };
            recordActivity(deps.activity, { level: "info", event: "compaction", session: event.sessionID, project: deps.project, detail: { mode: sameModelCompaction ? "same-model" : "switch", model: modelKey, chars: summary.length, durationMs: Date.now() - started } });
        });
    }

    function onContext<M extends ConversationMessage, S extends SystemText>(event: { sessionID: string; model: ModelRef; system: S[]; messages: M[] }): Promise<void> {
        return serialize(event.sessionID, async () => {
            event.messages = dedupeCompactionSummary(event.messages);
            let state = await load(event.sessionID);
            let changed = false;
            if (!state.origin) {
                const messages = await deps.loadContext(event.sessionID);
                const origin = historyModel(messages) ?? event.model;
                state = { ...state, origin: { providerID: origin.providerID, id: origin.id } };
                changed = true;
            }
            if (!state.lastModel) {
                state = { ...state, lastModel: state.origin };
                changed = true;
            }
            if (Object.keys(state.snapshots).length === 0 && sameModel(event.model, state.lastModel!)) {
                if (changed) {
                    await persist(event.sessionID, state);
                }
                return;
            }
            const modelKey = `${event.model.providerID}/${event.model.id}`;
            let snapshot = state.snapshots[modelKey];
            const now = (deps.now ?? Date.now)();
            const switched = !sameModel(event.model, state.lastModel!);
            const previousModel = state.lastModel!;
            let reused = true;
            if (!snapshot || (switched && snapshotIsStale(snapshot, event.messages, now))) {
                reused = false;
                try {
                    await deps.flush(event.sessionID);
                } catch (error) {
                    deps.log(`Failed to flush session ${event.sessionID} before snapshot`, error);
                    return;
                }
                const digest = await deps.client.digest({
                    harness: deps.harness,
                    project: deps.project,
                    session: event.sessionID,
                    budget: deps.digestBudget,
                });
                let cutMessageID: string | null = null;
                for (const message of event.messages) {
                    if (message.role === "user" && typeof message.id === "string") {
                        cutMessageID = message.id;
                    }
                }
                snapshot = { cutMessageID, digest: digest.text };
            }
            let lastMessageID: string | undefined;
            for (const message of event.messages) {
                if (typeof message.id === "string") {
                    lastMessageID = message.id;
                }
            }
            snapshot = { ...snapshot, lastMessageID, lastRequestAt: now };
            state = {
                ...state,
                lastModel: { providerID: event.model.providerID, id: event.model.id },
                snapshots: { ...state.snapshots, [modelKey]: snapshot },
            };
            await persist(event.sessionID, state);
            if (switched) {
                recordActivity(deps.activity, { level: "info", event: "switch", session: event.sessionID, project: deps.project, detail: { fromModel: `${previousModel.providerID}/${previousModel.id}`, toModel: modelKey, reused, chars: snapshot.digest.length } });
            }
            if (snapshot.compacted) {
                return;
            }
            const rewritten = rewriteForSwitch(event, snapshot, {
                tailUserTurns: deps.tailUserTurns,
                tailCharBudget: deps.tailCharBudget,
                cloneMessage,
            });
            event.system = rewritten.system;
            event.messages = rewritten.messages;
        });
    }

    async function forget(sessionID: string): Promise<void> {
        while (chains.has(sessionID)) {
            await chains.get(sessionID);
        }
        states.delete(sessionID);
    }

    return { onModelSelected, onCompaction, onContext, forget };
}
