import { Plugin } from "@opencode/plugin";
import { MementoClient } from "./client";
import { createContinueCommand, createHandoffCommand } from "./commands";
import { createIngestor } from "./ingest";
import { createJevGate, type DecisionsFn } from "./jev";
import { resolveOptions } from "./options";
import { createSessionOwnership } from "./session-ownership";
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

export function projectKey(project: { id: string; canonical?: unknown }): string {
    return typeof project.canonical === "string" && project.canonical.length > 0 ? project.canonical : project.id;
}

export default Plugin.define({
    id: "memento",
    async setup(ctx) {
        const options = resolveOptions(ctx.options);
        if (!options.enabled) {
            return () => {};
        }
        const project = projectKey(ctx.location.project);
        const client = new MementoClient({ url: options.url, token: options.token });
        const log = (message: string, error?: unknown) => console.warn("[memento] " + message, error ?? "");
        const ownsSession = createSessionOwnership({
            getSessionProjectID: async (sessionID) => (await ctx.session.get({ sessionID })).projectID,
            projectID: ctx.location.project.id,
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
                if (!await ownsSession(context.sessionID)) {
                    return { content: "memory_search: memento is not enabled for this project." };
                }
                return tool.execute(input, context);
            },
        }));
        const commandRegistration = await ctx.command.transform((editor) => {
            for (const entry of [command, handoff]) {
                editor.add({
                    ...entry,
                    async execute(input) {
                        if (!await ownsSession(input.sessionID)) {
                            throw new Error("memento is not enabled for this project");
                        }
                        await entry.execute(input);
                    },
                });
            }
        });
        const ensureRegistered = async (sessionID: string) => {
            try {
                await ingestor.ensureRegistered(sessionID);
            } catch (error) {
                log(`Failed to register session ${sessionID}`, error);
            }
        };
        const hookRegistration = await ctx.session.hook("context", async (event) => {
            if (!await ownsSession(event.sessionID)) {
                return;
            }
            await ensureRegistered(event.sessionID);
            await switchController.onContext(event);
        });
        const compactionRegistration = await ctx.session.hook("compaction", async (event) => {
            if (!await ownsSession(event.sessionID)) {
                return;
            }
            await switchController.onCompaction(event);
        });
        const controller = new AbortController();
        const eventLoop = (async () => {
            while (!controller.signal.aborted) {
                try {
                    for await (const event of ctx.event.subscribe({
                        types: ["session.execution.succeeded", "session.step.ended", "session.model.selected"],
                        signal: controller.signal,
                    })) {
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
            const disposals = Promise.allSettled([hookRegistration.dispose(), compactionRegistration.dispose(), toolRegistration.dispose(), commandRegistration.dispose()]);
            await eventLoop;
            await ingestor.idle();
            const results = await disposals;
            const failure = results.find((result) => result.status === "rejected");
            if (failure?.status === "rejected") {
                throw failure.reason;
            }
        };
    },
});
