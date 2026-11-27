import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { DecisionsFn } from "./jev";

type SystemOneOptions = {
    url: string;
    model: string;
    apiKeyEnv?: string;
    apiKeyFile?: string;
};

function record(value: unknown): Record<string, unknown> | undefined {
    return value !== null && typeof value === "object" && !Array.isArray(value)
        ? value as Record<string, unknown> : undefined;
}

function retryAfterMilliseconds(response: Response, body: unknown): number | undefined {
    const header = response.headers.get("retry-after");
    if (header?.trim()) {
        const seconds = Number(header);
        if (Number.isFinite(seconds) && seconds >= 0) {
            return seconds * 1000;
        }
        const date = Date.parse(header);
        if (Number.isFinite(date)) {
            return Math.max(0, date - Date.now());
        }
    }
    const pending: unknown[] = [body];
    const seen = new Set<unknown>();
    while (pending.length && seen.size < 32) {
        const value = pending.shift();
        const entry = record(value);
        if (!entry || seen.has(value)) {
            continue;
        }
        seen.add(value);
        for (const [field, multiplier] of [["retryAfterMs", 1], ["retryAfter", 1000]] as const) {
            const number = entry[field];
            if (typeof number === "number" && Number.isFinite(number) && number >= 0 && Number.isFinite(number * multiplier)) {
                return number * multiplier;
            }
        }
        pending.push(entry.error, entry.data, entry.cause);
    }
    return undefined;
}

async function readApiKey(options: SystemOneOptions): Promise<string | undefined> {
    if (options.apiKeyEnv && options.apiKeyFile) {
        throw new Error("System One requires only one API key source");
    }
    if (options.apiKeyEnv) {
        const key = process.env[options.apiKeyEnv]?.trim();
        if (!key || /[\r\n]/u.test(key)) {
            throw new Error("System One API key environment variable is missing or invalid");
        }
        return key;
    }
    if (options.apiKeyFile) {
        const path = options.apiKeyFile === "~" ? homedir()
            : options.apiKeyFile.startsWith("~/") ? join(homedir(), options.apiKeyFile.slice(2))
                : options.apiKeyFile;
        let key: string;
        try {
            key = (await readFile(path, "utf8")).trim();
        } catch {
            throw new Error("System One API key file could not be read");
        }
        if (!key || /[\r\n]/u.test(key)) {
            throw new Error("System One API key file is empty or invalid");
        }
        return key;
    }
    return undefined;
}

export function createSystemOneDecisions(options: SystemOneOptions): DecisionsFn {
    return async (input, signal) => {
        const key = await readApiKey(options);
        const questions = Object.fromEntries(input.questions.map((question) => [
            question.name,
            question.type === "predicate"
                ? { type: "noul", instructions: question.instructions }
                : {
                    type: "choice",
                    instructions: question.instructions,
                    criteria: Object.fromEntries(question.choices.map((choice) => [
                        String(choice.value), choice.description ?? String(choice.value),
                    ])),
                },
        ]));
        let response: Response;
        try {
            response = await fetch(options.url, {
                method: "POST",
                headers: {
                    "Content-Type": "application/json",
                    "User-Agent": "memento-opencode/plugin",
                    ...(key ? { Authorization: `Bearer ${key}` } : {}),
                },
                body: JSON.stringify({
                    model: options.model,
                    state: input.input.map((part) => part.text).join("\n"),
                    questions,
                }),
                signal,
                redirect: "error",
            });
        } catch {
            throw new Error("System One request failed");
        }
        if (!response.ok) {
            const body: unknown = await response.json().catch(() => undefined);
            const reason = response.status === 429 ? "rate_limit" : `http_${response.status}`;
            throw Object.assign(new Error(`System One request failed: ${reason}`), {
                status: response.status,
                reason,
                retryAfterMs: retryAfterMilliseconds(response, body),
            });
        }
        let body: unknown;
        try {
            body = await response.json();
        } catch {
            throw new Error("System One response could not be read");
        }
        const answers = record(record(body)?.answers);
        return {
            answers: input.questions.map((question) => {
                const answer = record(answers?.[question.name]);
                if (question.type === "predicate" && answer?.type === "noul"
                    && typeof answer.noul === "number" && Number.isFinite(answer.noul)
                    && answer.noul >= 0 && answer.noul <= 1) {
                    return { type: "predicate" as const, name: question.name, probability: answer.noul };
                }
                if (question.type === "choice" && answer?.type === "choice"
                    && typeof answer.choice === "string"
                    && (answer.confidence === undefined || (typeof answer.confidence === "number"
                        && Number.isFinite(answer.confidence) && answer.confidence >= 0 && answer.confidence <= 1))) {
                    const choice = question.choices.find((entry) => String(entry.value) === answer.choice);
                    if (choice) {
                        return {
                            type: "choice" as const,
                            name: question.name,
                            choice: choice.value,
                            ...(typeof answer.confidence === "number" ? { confidence: answer.confidence } : {}),
                        };
                    }
                }
                return { type: "error" as const, name: question.name, reason: "missing" };
            }),
        };
    };
}
