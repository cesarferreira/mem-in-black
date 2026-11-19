const connectionStatus = document.querySelector("#viewer-status");
const stats = document.querySelector("#viewer-stats");
const activity = document.querySelector("#viewer-activity");
const timestampFormat = new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
});
let etag;

function localizeTimestamps(container) {
    for (const time of container.querySelectorAll("time")) {
        const date = new Date(time.dateTime);
        if (!Number.isNaN(date.getTime())) time.textContent = timestampFormat.format(date);
    }
}

function updateActivity(next) {
    localizeTimestamps(next);
    const existing = new Map(Array.from(activity.children, (row) => [row.id, row]));
    let position = activity.firstElementChild;
    for (const incoming of [...next.children]) {
        const row = existing.get(incoming.id) ?? incoming;
        if (row !== incoming && row.innerHTML !== incoming.innerHTML) {
            row.replaceChildren(...incoming.childNodes);
        }
        if (row !== position) activity.insertBefore(row, position);
        position = row.nextElementSibling;
    }
    while (position) {
        const nextRow = position.nextElementSibling;
        position.remove();
        position = nextRow;
    }
}

function showStatus(connected) {
    const label = connected ? "Live" : "Reconnecting…";
    if (connectionStatus.textContent !== label) {
        connectionStatus.textContent = label;
        connectionStatus.dataset.connected = connected;
        connectionStatus.hidden = false;
    }
}

async function refresh() {
    if (!document.hidden) {
        try {
            const response = await fetch(location.href, {
                cache: "no-store",
                headers: etag ? { "If-None-Match": etag } : {},
                signal: AbortSignal.timeout(4000),
            });
            if (response.status !== 304) {
                if (!response.ok) throw new Error(`Viewer returned ${response.status}`);
                const page = new DOMParser().parseFromString(await response.text(), "text/html");
                const nextStats = page.querySelector("#viewer-stats");
                if (!stats.isEqualNode(nextStats)) stats.replaceChildren(...nextStats.childNodes);
                updateActivity(page.querySelector("#viewer-activity"));
                etag = response.headers.get("ETag");
            }
            showStatus(true);
        } catch {
            showStatus(false);
        }
    }
    setTimeout(refresh, 1000);
}

localizeTimestamps(activity);
refresh();
