import { Plugin } from "@opencode/plugin";
import { MementoClient } from "./client";
import { createContinueCommand, createHandoffCommand } from "./commands";
import { createIngestor } from "./ingest";
import { createJevGate, type DecisionsFn } from "./jev";
import { resolveOptions } from "./options";
import { createSessionOwnership, normalizeDirectory } from "./session-ownership";
import { createSubagentMemoryController } from "./subagent-memory";
import { cloneMessage, type ConversationMessage, type SystemText } from "./switch";
import { createSwitchController } from "./switch-controller";
import { createMemorySearchTool } from "./tools";

function waitForRetry(signal: AbortSignal): Promise<void> {
    return new Promise((resolve) => {
        if (signal.aborted) {
            resolve();
            return;
        }
        const finish = () => {
            clearTimeout(timer);
            signal.removeEventListener("abort", finish);
            resolve();
        };
        const timer = setTimeout(finish, 1000);
        signal.addEventListener("abort", finish, { once: true });
    });
}

export function projectKey(project: { id: string; canonical?: unknown }, directory: string): string {
    if (project.id === "global") {
        return normalizeDirectory(directory);
    }
    return typeof project.canonical === "string" && project.canonical.length > 0 ? project.canonical : project.id;
}

function isSessionNotFound(error: unknown): boolean {
    return Boolean(error && typeof error === "object" && "_tag" in error && error._tag === "SessionNotFoundError")
        || String(error).includes("NotFoundError");
}

function injectContinuation<M extends ConversationMessage, S extends SystemText>(
    event: { system: S[]; messages: M[] },
    continuation: string,
): void {
    const containsContinuation = (part: unknown) => Boolean(
        part && typeof part === "object" && "text" in part
        && typeof part.text === "string" && part.text.includes("<memento-continuation"),
    );
    if (event.system.some(containsContinuation) || event.messages.some((message) => message.content.some(containsContinuation))) {
        return;
    }
    const part = { type: "text", text: continuation };
    const userIndex = event.messages.findIndex((message) => message.role === "user");
    if (userIndex < 0) {
        event.system = [...event.system, part as S];
        return;
    }
    const original = event.messages[userIndex];
    event.messages = [...event.messages];
    event.messages[userIndex] = cloneMessage(original, [part, ...original.content]);
}

export default Plugin.define({
    id: "memento",
    async setup(ctx) {
        const options = resolveOptions(ctx.options);
        if (!options.enabled) {
            return () => {};
        }
        const project = projectKey(ctx.location.project, ctx.location.directory);
        const client = new MementoClient({ url: options.url, token: options.token });
        const log = (message: string, error?: unknown) => console.warn("[memento] " + message, error ?? "");
        const ownsSession = createSessionOwnership({
            getSession: (sessionID) => ctx.session.get({ sessionID }),
            projectID: ctx.location.project.id,
            directory: ctx.location.directory,
            log,
        });
        const decide: DecisionsFn | undefined = options.jev
            ? createJevGate({
                decide: (input) => ctx.experimental.generate.decisions(input),
                cooldownMs: options.jevCooldownMs,
                log,
            })
            : undefined;
        const ingestor = createIngestor({
            client,
            storage: ctx.storage,
            loadContext: (sessionID) => ctx.session.context({ sessionID }).catch(() => []),
            loadSession: (sessionID) => ctx.session.get({ sessionID }),
            putInstruction: (sessionID, key, value) => ctx.session.instructions.entry.put({ sessionID, key, value }),
            decide,
            jevModel: options.jevModel,
            harness: options.harness,
            project,
            log,
        });
        const switchController = createSwitchController({
            client,
            storage: ctx.storage,
            flush: ingestor.flush,
            loadContext: (sessionID) => ctx.session.context({ sessionID }).catch(() => []),
            harness: options.harness,
            project,
            digestBudget: options.digestBudget,
            tailUserTurns: options.tailUserTurns,
            tailCharBudget: options.tailCharBudget,
            log,
        });
        const subagentMemory = createSubagentMemoryController({
            mode: options.subagentMemory,
            agents: options.subagentMemoryAgents,
            budget: options.subagentDigestBudget,
            harness: options.harness,
            project,
            lookup: ownsSession.lookup,
            root: ownsSession.root,
            flush: ingestor.flush,
            client,
            storage: ctx.storage,
            log,
        });
        const tool = createMemorySearchTool({ client, decide, jevModel: options.jevModel, harness: options.harness, project });
        const command = createContinueCommand({
            client,
            putInstruction: (sessionID, key, value) => ctx.session.instructions.entry.put({ sessionID, key, value }),
            markRegistered: ingestor.markRegistered,
            harness: options.harness,
            project,
            digestBudget: options.digestBudget,
        });
        const handoff = createHandoffCommand({
            client,
            flush: ingestor.flush,
            putInstruction: (sessionID, key, value) => ctx.session.instructions.entry.put({ sessionID, key, value }),
            harness: options.harness,
            project,
        });
        const toolRegistration = await ctx.tool.transform((editor) => editor.add({
            ...tool,
            async execute(input, context) {
                const sessionID = await ownsSession.root(context.sessionID);
                if (!sessionID) {
                    return { content: "memory_search: memento is not enabled for this project." };
                }
                return tool.execute(input, { ...context, sessionID });
            },
        }));
        const commandRegistration = await ctx.command.transform((editor) => {
            for (const entry of [command, handoff]) {
                editor.add({
                    ...entry,
                    async execute(input) {
                        const ownership = await ownsSession.lookup(input.sessionID);
                        if (!ownership?.owned) {
                            throw new Error("memento is not enabled for this project");
                        }
                        if (ownership.parentID !== undefined) {
                            throw new Error(`${entry.name} is not available in subagent sessions`);
                        }
                        await entry.execute(input);
                    },
                });
            }
        });
        const ensureRegistered = async (sessionID: string) => {
            try {
                return await ingestor.ensureRegistered(sessionID);
            } catch (error) {
                log(`Failed to register session ${sessionID}`, error);
            }
        };
        const hookRegistration = await ctx.session.hook("context", async (event) => {
            if (!await ownsSession(event.sessionID)) {
                await subagentMemory.onContext(event);
                return;
            }
            const continuation = await ensureRegistered(event.sessionID);
            if (continuation) {
                injectContinuation(event, continuation);
            }
            await switchController.onContext(event);
            if (continuation) {
                injectContinuation(event, continuation);
            }
        });
        const compactionRegistration = await ctx.session.hook("compaction", async (event) => {
            if (!await ownsSession(event.sessionID)) {
                return;
            }
            await switchController.onCompaction(event);
        });
        const controller = new AbortController();
        const releaseSession = async (sessionID: string, reason?: string, harness = options.harness) => {
            try {
                await subagentMemory.forget(sessionID);
                await switchController.forget(sessionID);
                await ingestor.forget(sessionID);
                await client.releaseSession({ harness, project, session: sessionID, ...(reason ? { reason } : {}) });
            } catch (error) {
                log(`Failed to release session ${sessionID}`, error);
            } finally {
                for (const key of ["ingest", "registered", "fork", "claimed", "parent", "switch", "subagent"]) {
                    try {
                        await ctx.storage.remove(`memento/${key}/${sessionID}`);
                    } catch (error) {
                        log(`Failed to delete ${key} state for session ${sessionID}`, error);
                    }
                }
                ownsSession.forget(sessionID);
            }
        };
        const reconcileSessions = async () => {
            try {
                const { sessions } = await client.listSessions(options.harness, project);
                for (const session of sessions) {
                    if (controller.signal.aborted) {
                        return;
                    }
                    if (session.harness !== options.harness || session.project !== project || typeof session.session !== "string") {
                        continue;
                    }
                    let reason: string | undefined;
                    try {
                        const current = await ctx.session.get({ sessionID: session.session });
                        if (ownsSession.isOwned(current) && current.parentID !== undefined) {
                            reason = "subagent";
                        }
                    } catch (error) {
                        if (isSessionNotFound(error)) {
                            reason = "deleted";
                        }
                    }
                    if (reason && !controller.signal.aborted) {
                        await releaseSession(session.session, reason, options.harness);
                    }
                }
            } catch (error) {
                if (!controller.signal.aborted) {
                    log("Failed to reconcile sessions", error);
                }
            }
        };
        let reconciliation: Promise<void> | undefined;
        const startReconciliation = () => {
            if (!reconciliation && !controller.signal.aborted) {
                reconciliation = reconcileSessions().finally(() => { reconciliation = undefined; });
            }
        };
        const reconcileTimer = options.reconcile ? setInterval(startReconciliation, 24 * 60 * 60 * 1000) : undefined;
        if (options.reconcile) {
            startReconciliation();
        }
        const eventLoop = (async () => {
            while (!controller.signal.aborted) {
                try {
                    for await (const event of ctx.event.subscribe({
                        types: ["session.execution.succeeded", "session.step.ended", "session.model.selected", "session.deleted"],
                        signal: controller.signal,
                    })) {
                        if (event.type === "session.deleted") {
                            const sessionID = event.data.sessionID;
                            await releaseSession(sessionID);
                            continue;
                        }
                        if (!await ownsSession(event.data.sessionID)) {
                            continue;
                        }
                        if (event.type === "session.execution.succeeded" || event.type === "session.step.ended") {
                            void ingestor.handleExecutionSucceeded(event.data.sessionID);
                        } else if (event.type === "session.model.selected") {
                            await ensureRegistered(event.data.sessionID);
                            await switchController.onModelSelected(event.data.sessionID, event.data.model, event.data.previous);
                        }
                    }
                    if (!controller.signal.aborted) {
                        await waitForRetry(controller.signal);
                    }
                } catch (error) {
                    if (!controller.signal.aborted) {
                        log("Event subscription failed", error);
                        await waitForRetry(controller.signal);
                    }
                }
            }
        })();
        return async () => {
            controller.abort();
            if (reconcileTimer !== undefined) {
                clearInterval(reconcileTimer);
            }
            const disposals = Promise.allSettled([hookRegistration.dispose(), compactionRegistration.dispose(), toolRegistration.dispose(), commandRegistration.dispose()]);
            await eventLoop;
            await reconciliation;
            await ingestor.idle();
            const results = await disposals;
            const failure = results.find((result) => result.status === "rejected");
            if (failure?.status === "rejected") {
                throw failure.reason;
            }
        };
    },
});
