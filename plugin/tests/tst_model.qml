import QtQuick
import QtTest
import "../Model.js" as Model

TestCase {
    name: "Model"

    function fixture(name) {
        var request = new XMLHttpRequest()
        request.open("GET", Qt.resolvedUrl("fixtures/" + name), false)
        request.send()
        compare(request.status, 200)
        return JSON.parse(request.responseText)
    }

    function snapshot() {
        var value = fixture("status-full.json")
        value.generated_at = 1000
        value.config_errors = []
        // The golden file covers edge cases; the level tests start from one healthy root.
        value.roots = [{"name": "source", "tier": "public", "state": "ok", "reason": null,
                        "last_scan": 1000, "queued": 0, "done": 10,
                        "skipped_symlinks": 0, "files": 10, "chunks": 20}]
        return value
    }

    function ready() {
        var state = Model.applySnapshot(Model.empty(), snapshot())
        state.unit = Model.unitState("ActiveState=active\nSubState=running\nResult=success\n")
        return state
    }

    function test_constants_and_formatting() {
        compare(Model.BACKLOG, 1000)
        compare(Model.BRIEF_PERIOD_S, 60)
        compare(Model.STALE_S, 180)
        compare(Model.fmtCount(12345), "12,345")
        compare(Model.fmtCount(null), "—")
        compare(Model.fmtBytes(1024), "1.0 KiB")
        compare(Model.fmtAge(null), "never")
        compare(Model.fmtAge(120), "2m ago")
    }

    function test_levels_and_priority() {
        var state = ready()
        compare(Model.level(state, 1000), "ok")
        compare(Model.tooltip("ok"), "llm-index: ok")

        state.live.roots[0].queued = Model.BACKLOG
        compare(Model.level(state, 1000), "indexing")
        compare(Model.tooltip("indexing"), "llm-index: indexing")
        state.live.roots[0].queued = 0
        state.live.roots[0].last_scan = null
        compare(Model.level(state, 1000), "indexing")
        state.live.roots[0].last_scan = 1000
        state.live.roots[0].state = "pending"
        compare(Model.level(state, 1000), "indexing")

        var badStates = ["offline", "degraded", "config_error"]
        for (var i = 0; i < badStates.length; i++) {
            state.live.roots[0].state = badStates[i]
            compare(Model.level(state, 1000), "problem")
        }
        state.live.roots[0].state = "pending"
        state.live.config_errors = [{ root: null, what: "unknown", reason: "other" }]
        compare(Model.level(state, 1000), "problem")
        state.live.config_errors = []
        state.briefError = "status failed"
        compare(Model.level(state, 1000), "problem")
        compare(Model.tooltip("problem"), "llm-index: needs attention")

        var down = ["stopped", "failed", "unknown"]
        for (var j = 0; j < down.length; j++) {
            state.unit.state = down[j]
            compare(Model.level(state, 1000), "down")
        }
        compare(Model.tooltip("down"), "llm-index: service not running")
        state.unit.state = "running"
        state.briefError = null
        state.live = null
        compare(Model.level(state, 1000), "problem")
    }

    function test_stale_boundaries() {
        var state = ready()
        compare(Model.level(state, 1000 + Model.STALE_S - 1), "ok")
        compare(Model.level(state, 1000 + Model.STALE_S), "ok")
        compare(Model.level(state, 1000 + Model.STALE_S + 1), "problem")
    }

    function test_snapshot_order_and_counts() {
        var full = snapshot()
        var state = Model.applySnapshot(Model.empty(), full)
        compare(state.full.roots[0].files, 10)
        var brief = snapshot()
        brief.brief = true
        brief.generated_at = 1010
        brief.roots[0].files = null
        brief.roots[0].chunks = null
        brief.roots[0].queued = 3
        state = Model.applySnapshot(state, brief)
        compare(state.liveAt, 1010)
        compare(state.live.roots[0].queued, 3)
        compare(state.fullAt, 1000)
        compare(state.full.roots[0].files, 10)

        var older = snapshot()
        older.generated_at = 900
        older.roots[0].queued = 9
        state = Model.applySnapshot(state, older)
        compare(state.liveAt, 1010)
        compare(state.live.roots[0].queued, 3)
        compare(state.fullAt, 1000)

        state = Model.empty()
        state = Model.applySnapshot(state, brief)
        compare(state.full, null)
        state = Model.applySnapshot(state, full)
        compare(state.liveAt, 1010)
        compare(state.fullAt, 1000)
        compare(state.full.roots[0].files, 10)
        var staleBrief = fixture("status-brief.json")
        state = Model.applySnapshot(state, staleBrief)
        compare(state.liveAt, 1010)
        compare(state.full.roots[0].files, 10)
    }

    function test_unit_mapping() {
        var states = ["active", "activating", "deactivating", "reloading", "failed", "inactive", "other"]
        var mapped = ["running", "starting", "starting", "starting", "failed", "stopped", "unknown"]
        for (var i = 0; i < states.length; i++) {
            var unit = Model.unitState("ActiveState=" + states[i] + "\nSubState=running\nResult=success\n")
            compare(unit.state, mapped[i])
            compare(unit.sub, "running")
            compare(unit.result, "success")
        }
        compare(Model.unitState("").state, "unknown")
        compare(Model.unitState("garbage\nSubState=nope").state, "unknown")
    }

    function test_failures_and_recovery() {
        var state = Model.empty()
        state.unit = Model.unitState("ActiveState=active")
        state = Model.applyError(state, "brief", "not installed")
        compare(Model.level(state, 0), "problem")
        compare(state.briefError, "not installed")
        state = Model.applySnapshot(state, snapshot())
        compare(state.briefError, "not installed")
        compare(Model.level(state, 1000), "problem")
        state = Model.applyError(state, "full", "timed out")
        compare(state.fullError, "timed out")
        var brief = snapshot()
        brief.brief = true
        brief.roots[0].files = null
        brief.roots[0].chunks = null
        state = Model.applySnapshot(state, brief)
        compare(state.briefError, null)
        compare(state.fullError, "timed out")
        compare(Model.level(state, 1000), "ok")
        state = Model.applySnapshot(state, snapshot())
        compare(state.fullError, null)
        state = Model.applyError(state, "brief", "bad output")
        compare(Model.level(state, 1000), "problem")
        state = Model.applySnapshot(state, brief)
        compare(Model.level(state, 1000), "ok")
    }

    function test_validate() {
        verify(Model.validate(fixture("status-full.json")) !== null)
        verify(Model.validate(fixture("status-brief.json")) !== null)
        var value = snapshot()
        value.roots[0].queued = "0"
        compare(Model.validate(value), null)
        value = snapshot()
        value.roots[0].state = "surprise"
        compare(Model.validate(value), null)
        value = snapshot()
        value.settings.limits.workers = null
        compare(Model.validate(value), null)
        value = fixture("status-brief.json")
        value.roots[0].files = 1
        compare(Model.validate(value), null)
        compare(Model.validate({}), null)
    }
}
