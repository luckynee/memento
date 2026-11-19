export function createSessionOwnership(deps: {
    getSessionProjectID(sessionID: string): Promise<string>;
    projectID: string;
    log(message: string, error?: unknown): void;
}) {
    const sessions = new Map<string, boolean>();
    const pending = new Map<string, Promise<boolean>>();

    const ownsSession = async (sessionID: string): Promise<boolean> => {
        const cached = sessions.get(sessionID);
        if (cached !== undefined) {
            sessions.delete(sessionID);
            sessions.set(sessionID, cached);
            return cached;
        }
        const existing = pending.get(sessionID);
        if (existing) {
            return existing;
        }
        const lookup = Promise.resolve().then(async () => {
            try {
                const projectID = await deps.getSessionProjectID(sessionID);
                if (typeof projectID !== "string" || !projectID) {
                    throw new Error("Session project ID is missing");
                }
                const owned = projectID === deps.projectID;
                if (pending.get(sessionID) === lookup) {
                    sessions.delete(sessionID);
                    sessions.set(sessionID, owned);
                    if (sessions.size > 64) {
                        sessions.delete(sessions.keys().next().value!);
                    }
                }
                return owned;
            } catch (error) {
                deps.log(`Failed to determine project for session ${sessionID}`, error);
                return false;
            }
        });
        pending.set(sessionID, lookup);
        try {
            return await lookup;
        } finally {
            if (pending.get(sessionID) === lookup) {
                pending.delete(sessionID);
            }
        }
    };

    ownsSession.forget = (sessionID: string): void => {
        sessions.delete(sessionID);
        pending.delete(sessionID);
    };

    return ownsSession;
}
