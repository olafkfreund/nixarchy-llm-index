.pragma library

var BACKLOG = 1000
var BRIEF_PERIOD_S = 60
var STALE_S = 180

function empty() {
    return { live: null, liveAt: null, full: null, fullAt: null,
             unit: { state: "unknown", sub: "", result: "" },
             briefError: null, fullError: null, busy: false }
}

function integer(value) {
    return typeof value === "number" && isFinite(value) && Math.floor(value) === value && value >= 0
}

function optionalInteger(value) {
    return value === null || integer(value)
}

function object(value) {
    return value !== null && typeof value === "object" && !Array.isArray(value)
}

function validate(value) {
    if (!object(value) || !integer(value.generated_at) || typeof value.brief !== "boolean" ||
            !Array.isArray(value.roots) || !Array.isArray(value.config_errors) ||
            !object(value.settings) || typeof value.sandbox !== "string" ||
            !Array.isArray(value.databases))
        return null

    for (var i = 0; i < value.roots.length; i++) {
        var root = value.roots[i]
        if (!object(root) || typeof root.name !== "string" || typeof root.tier !== "string" ||
                ["ok", "offline", "degraded", "config_error", "pending"].indexOf(root.state) < 0 ||
                !(root.reason === null || typeof root.reason === "string") ||
                !optionalInteger(root.last_scan) || !optionalInteger(root.queued) ||
                !optionalInteger(root.done) || !optionalInteger(root.skipped_symlinks) ||
                !optionalInteger(root.files) || !optionalInteger(root.chunks) ||
                (value.brief ? (root.files !== null || root.chunks !== null)
                             : (!integer(root.files) || !integer(root.chunks))))
            return null
    }
    for (var j = 0; j < value.config_errors.length; j++) {
        var error = value.config_errors[j]
        if (!object(error) || !(error.root === null || typeof error.root === "string") ||
                ["root", "exclusion", "unknown"].indexOf(error.what) < 0 ||
                !(error.reason === null || typeof error.reason === "string"))
            return null
    }
    var settings = value.settings
    if (!Array.isArray(settings.roots) || !integer(settings.exclusions) ||
            !integer(settings.lifted_builtin) || !object(settings.limits))
        return null
    for (var k = 0; k < settings.roots.length; k++) {
        var configured = settings.roots[k]
        if (!object(configured) || typeof configured.name !== "string" ||
                typeof configured.tier !== "string" || typeof configured.kind !== "string" ||
                typeof configured.interval !== "string" || typeof configured.share !== "boolean")
            return null
    }
    var limits = settings.limits
    if (!integer(limits.max_file_mb) || !integer(limits.max_text_bytes) ||
            !integer(limits.max_cells) || !integer(limits.memory_max_mb) ||
            !integer(limits.workers) || typeof limits.backoff_max !== "string")
        return null
    for (var n = 0; n < value.databases.length; n++) {
        var db = value.databases[n]
        if (!object(db) || typeof db.name !== "string" || !integer(db.bytes))
            return null
    }
    return value
}

function applySnapshot(st, snap) {
    var next = Object.assign({}, st)
    if (snap.generated_at >= st.liveAt || st.live === null) {
        next.live = { roots: snap.roots, config_errors: snap.config_errors }
        next.liveAt = snap.generated_at
    }
    if (snap.brief) {
        next.briefError = null
    } else {
        if (snap.generated_at >= st.fullAt || st.full === null) {
            next.full = { roots: snap.roots, settings: snap.settings,
                          sandbox: snap.sandbox, databases: snap.databases }
            next.fullAt = snap.generated_at
        }
        next.fullError = null
    }
    return next
}

function applyError(st, kind, category) {
    var next = Object.assign({}, st)
    if (kind === "brief")
        next.briefError = category
    else if (kind === "full")
        next.fullError = category
    return next
}

function unitState(text) {
    var fields = {}
    var lines = text.split(/\r?\n/)
    for (var i = 0; i < lines.length; i++) {
        var match = /^(ActiveState|SubState|Result)=(.*)$/.exec(lines[i])
        if (match)
            fields[match[1]] = match[2]
    }
    var state = "unknown"
    if (fields.ActiveState === "active")
        state = "running"
    else if (["activating", "deactivating", "reloading"].indexOf(fields.ActiveState) >= 0)
        state = "starting"
    else if (fields.ActiveState === "failed")
        state = "failed"
    else if (fields.ActiveState === "inactive")
        state = "stopped"
    return { state: state, sub: fields.SubState || "", result: fields.Result || "" }
}

function level(st, now) {
    if (["stopped", "failed", "unknown"].indexOf(st.unit.state) >= 0)
        return "down"
    if (!st.live || st.briefError || now - st.liveAt > STALE_S ||
            st.live.config_errors.length > 0)
        return "problem"
    var indexing = false
    for (var i = 0; i < st.live.roots.length; i++) {
        var root = st.live.roots[i]
        if (["offline", "degraded", "config_error"].indexOf(root.state) >= 0)
            return "problem"
        if (root.state === "pending" || root.last_scan === null || root.queued >= BACKLOG)
            indexing = true
    }
    return indexing ? "indexing" : "ok"
}

function tooltip(value) {
    if (value === "down") return "llm-index: service not running"
    if (value === "problem") return "llm-index: needs attention"
    if (value === "indexing") return "llm-index: indexing"
    return "llm-index: ok"
}

function fmtCount(value) {
    if (value === null || value === undefined) return "—"
    return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ",")
}

function fmtBytes(value) {
    if (value === null || value === undefined) return "—"
    var units = ["B", "KiB", "MiB", "GiB", "TiB"]
    var size = value
    var unit = 0
    while (size >= 1024 && unit < units.length - 1) {
        size /= 1024
        unit++
    }
    return (unit === 0 ? size : size.toFixed(1)) + " " + units[unit]
}

function fmtAge(sec) {
    if (sec === null || sec === undefined) return "never"
    if (sec < 60) return Math.floor(sec) + "s ago"
    if (sec < 3600) return Math.floor(sec / 60) + "m ago"
    if (sec < 86400) return Math.floor(sec / 3600) + "h ago"
    return Math.floor(sec / 86400) + "d ago"
}
