export type IngestItem = {
    source_id: string;
    ts: string;
    role: "user" | "assistant" | "tool";
    text: string;
    kind_hint: string | null;
    tool: { name: string; input: unknown; output: string; ok: boolean } | null;
};

type RecordValue = Record<string, unknown>;

function record(value: unknown): RecordValue | undefined {
    return value !== null && typeof value === "object" && !Array.isArray(value)
        ? value as RecordValue
        : undefined;
}

function timestamp(value: unknown): string | undefined {
    if (typeof value !== "number" || !Number.isFinite(value)) {
        return undefined;
    }
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? undefined : date.toISOString();
}

function renderContent(value: unknown): string {
    if (!Array.isArray(value)) {
        return "";
    }
    return value.flatMap((entry: unknown) => {
        const part = record(entry);
        if (part?.type === "text" && typeof part.text === "string") {
            return [part.text];
        }
        if (part?.type === "file" && typeof part.uri === "string") {
            return [`[file ${typeof part.name === "string" ? part.name : part.uri}]`];
        }
        return [];
    }).join("\n");
}

export function mapMessage(value: unknown): IngestItem[] {
    const message = record(value);
    if (!message || typeof message.id !== "string") {
        return [];
    }
    const messageTimestamp = timestamp(record(message.time)?.created);
    if (!messageTimestamp) {
        return [];
    }
    if (message.type === "user") {
        if (typeof message.text !== "string" || !message.text.trim()) {
            return [];
        }
        return [{ source_id: message.id, ts: messageTimestamp, role: "user", text: message.text, kind_hint: null, tool: null }];
    }
    if (message.type !== "assistant" || !Array.isArray(message.content)) {
        return [];
    }
    const items: IngestItem[] = [];
    message.content.forEach((value: unknown, index: number) => {
        const part = record(value);
        if (part?.type === "text" && typeof part.text === "string" && part.text.trim()) {
            items.push({
                source_id: `${message.id}:text:${index}`,
                ts: messageTimestamp,
                role: "assistant",
                text: part.text,
                kind_hint: null,
                tool: null,
            });
        }
        if (part?.type !== "tool" || typeof part.id !== "string" || typeof part.name !== "string") {
            return;
        }
        const state = record(part.state);
        if (!state || (state.status !== "completed" && state.status !== "error")) {
            return;
        }
        const content = renderContent(state.content);
        const error = record(state.error);
        const errorMessage = typeof error?.message === "string" ? error.message : "";
        const output = state.status === "error" ? [errorMessage, content].filter(Boolean).join("\n") : content;
        items.push({
            source_id: `${message.id}:tool:${part.id}`,
            ts: timestamp(record(part.time)?.created) ?? messageTimestamp,
            role: "tool",
            text: "",
            kind_hint: null,
            tool: { name: part.name, input: state.input, output, ok: state.status === "completed" },
        });
    });
    return items;
}

export function selectNewMessages(messages: unknown[], lastIngestedId?: string): { messages: unknown[]; nextCursor?: string } {
    const cursorIndex = lastIngestedId === undefined ? -1 : messages.findIndex((message) => record(message)?.id === lastIngestedId);
    let nextCursor: string | undefined;
    for (const message of messages) {
        const id = record(message)?.id;
        if (typeof id === "string") {
            nextCursor = id;
        }
    }
    const start = cursorIndex < 0 ? 0 : cursorIndex + (record(messages[cursorIndex])?.type === "assistant" ? 0 : 1);
    return { messages: start === 0 ? messages : messages.slice(start), nextCursor };
}
