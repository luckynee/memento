type SessionInfo = { projectID: string; parentID?: string; location?: { directory?: string } };

export function createSessionOwnership(deps: {
    getSession(sessionID: string): Promise<SessionInfo>;
    projectID: string;
    directory: string;
    log(message: string, error?: unknown): void;
}) {
    type SessionOwnership = { owned: boolean; parentID?: string };
    const sessions = new Map<string, SessionOwnership>();
    const pending = new Map<string, Promise<SessionOwnership | undefined>>();

    const isOwned = (session: SessionInfo): boolean => {
        if (session.projectID !== deps.projectID) {
            return false;
        }
        if (deps.projectID !== "global") {
            return true;
        }
        const directory = session.location?.directory;
        return typeof directory === "string" && directory.length > 0
            && directory.replace(/\/$/, "") === deps.directory.replace(/\/$/, "");
    };

    const lookupSession = async (sessionID: string): Promise<SessionOwnership | undefined> => {
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
                const session = await deps.getSession(sessionID);
                if (typeof session.projectID !== "string" || !session.projectID) {
                    throw new Error("Session project ID is missing");
                }
                if (session.parentID !== undefined && (typeof session.parentID !== "string" || !session.parentID)) {
                    throw new Error("Session parent ID is invalid");
                }
                const ownership = { owned: isOwned(session), parentID: session.parentID };
                if (pending.get(sessionID) === lookup) {
                    sessions.delete(sessionID);
                    sessions.set(sessionID, ownership);
                    if (sessions.size > 64) {
                        sessions.delete(sessions.keys().next().value!);
                    }
                }
                return ownership;
            } catch (error) {
                deps.log(`Failed to determine project for session ${sessionID}`, error);
                return undefined;
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

    const ownsSession = async (sessionID: string): Promise<boolean> => {
        const ownership = await lookupSession(sessionID);
        return ownership?.owned === true && ownership.parentID === undefined;
    };

    ownsSession.lookup = lookupSession;
    ownsSession.isOwned = isOwned;
    ownsSession.root = async (sessionID: string): Promise<string | undefined> => {
        const visited = new Set<string>();
        let current = sessionID;
        while (visited.size < 64 && !visited.has(current)) {
            visited.add(current);
            const ownership = await lookupSession(current);
            if (!ownership?.owned) {
                return undefined;
            }
            if (ownership.parentID === undefined) {
                return current;
            }
            current = ownership.parentID;
        }
        return undefined;
    };
    ownsSession.forget = (sessionID: string): void => {
        sessions.delete(sessionID);
        pending.delete(sessionID);
    };

    return ownsSession;
}
