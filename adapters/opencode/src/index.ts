import { Plugin } from "@opencode/plugin";
import { MementoClient } from "./client";
import { createContinueCommand } from "./commands";
import { createIngestor } from "./ingest";
import { createJevGate, type DecisionsFn } from "./jev";
import { resolveOptions } from "./options";
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

export default Plugin.define({
    id: "memento",
    async setup(ctx) {
        const options = resolveOptions(ctx.options);
        if (!options.enabled) {
            return () => {};
        }
        const project = ctx.location.project.id;
        const client = new MementoClient({ url: options.url, token: options.token });
        const log = (message: string, error?: unknown) => console.warn("[memento] " + message, error ?? "");
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
            loadContext: (sessionID) => ctx.session.context({ sessionID }),
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
        const toolRegistration = await ctx.tool.transform((editor) => editor.add(tool));
        const commandRegistration = await ctx.command.transform((editor) => editor.add(command));
        const hookRegistration = await ctx.session.hook("context", (event) => switchController.onContext(event));
        const controller = new AbortController();
        const eventLoop = (async () => {
            while (!controller.signal.aborted) {
                try {
                    for await (const event of ctx.event.subscribe({
                        types: ["session.execution.succeeded", "session.step.ended", "session.model.selected"],
                        signal: controller.signal,
                    })) {
                        if (event.type === "session.execution.succeeded" || event.type === "session.step.ended") {
                            void ingestor.handleExecutionSucceeded(event.data.sessionID);
                        } else if (event.type === "session.model.selected") {
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
            const disposals = Promise.allSettled([hookRegistration.dispose(), toolRegistration.dispose(), commandRegistration.dispose()]);
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
