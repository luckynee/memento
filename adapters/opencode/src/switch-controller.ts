import type { MementoClient } from "./client";
import { cloneMessage, rewriteForSwitch, sameModel, type ConversationMessage, type ModelRef, type SwitchState, type SystemText } from "./switch";

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
    digestBudget: number;
    tailUserTurns: number;
    tailCharBudget: number;
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

    function onContext<M extends ConversationMessage, S extends SystemText>(event: { sessionID: string; model: ModelRef; system: S[]; messages: M[] }): Promise<void> {
        return serialize(event.sessionID, async () => {
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
            if (!snapshot) {
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
                state = { ...state, snapshots: { ...state.snapshots, [modelKey]: snapshot } };
            }
            state = { ...state, lastModel: { providerID: event.model.providerID, id: event.model.id } };
            await persist(event.sessionID, state);
            const rewritten = rewriteForSwitch(event, snapshot, {
                tailUserTurns: deps.tailUserTurns,
                tailCharBudget: deps.tailCharBudget,
                cloneMessage,
            });
            event.system = rewritten.system;
            event.messages = rewritten.messages;
        });
    }

    return { onModelSelected, onContext };
}
