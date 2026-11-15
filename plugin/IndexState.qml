pragma Singleton

import QtQuick
import Quickshell
import Quickshell.Io
import "Model.js" as Model

Singleton {
  id: root

  property var state: Model.empty()
  property int now: Math.floor(Date.now() / 1000)
  property int viewers: 0
  property int openPanels: 0

  readonly property string level: Model.level(root.state, root.now)
  readonly property string tooltipText: Model.tooltip(root.level)
  readonly property var live: root.state.live
  readonly property var liveAt: root.state.liveAt
  readonly property var full: root.state.full
  readonly property var fullAt: root.state.fullAt
  readonly property var unit: root.state.unit
  readonly property var briefError: root.state.briefError
  readonly property var fullError: root.state.fullError
  readonly property bool busy: root.state.busy

  function acquire() {
    root.viewers += 1
    root.now = Math.floor(Date.now() / 1000)
    root.pollUnit()
    root.pollBrief()
  }

  function release() {
    root.viewers = Math.max(0, root.viewers - 1)
  }

  function panelOpened() {
    root.openPanels += 1
    root.now = Math.floor(Date.now() / 1000)
    root.pollUnit()
    root.pollBrief()
    root.pollFull()
  }

  function panelClosed() {
    root.openPanels = Math.max(0, root.openPanels - 1)
  }

  function refresh() {
    root.now = Math.floor(Date.now() / 1000)
    root.pollFull()
  }

  function restartService() {
    if (restartProcess.running) return
    restartProcess.running = true
  }

  function openLog() {
    if (logProcess.running) return
    logProcess.running = true
  }

  function setBusy(flag) {
    if (root.state.busy === flag) return
    var next = Object.assign({}, root.state)
    next.busy = flag
    root.state = next
  }

  function setUnit(unitValue) {
    root.now = Math.floor(Date.now() / 1000)
    var next = Object.assign({}, root.state)
    next.unit = unitValue
    next.busy = fullProcess.running
    root.state = next
  }

  function recordError(kind, category) {
    root.now = Math.floor(Date.now() / 1000)
    var next = Model.applyError(root.state, kind, category)
    next.busy = fullProcess.running
    root.state = next
  }

  function recordSnapshot(kind, rawText) {
    root.now = Math.floor(Date.now() / 1000)
    var parsed = null
    try {
      parsed = Model.validate(JSON.parse(rawText))
    } catch (e) {
      parsed = null
    }
    if (parsed === null) {
      root.recordError(kind, "bad output")
      return
    }
    var next = Model.applySnapshot(root.state, parsed)
    next.busy = fullProcess.running
    root.state = next
  }

  function pollUnit() {
    if (unitProcess.running) return
    unitProcess.timedOut = false
    unitProcess.started = false
    unitProcess.handled = false
    unitDeadline.restart()
    unitProcess.running = true
    if (!unitProcess.running && !unitProcess.handled) {
      unitDeadline.stop()
      unitProcess.handled = true
      root.setUnit(Model.unitState(""))
    }
  }

  function pollBrief() {
    if (briefProcess.running) return
    briefProcess.timedOut = false
    briefProcess.started = false
    briefProcess.handled = false
    briefDeadline.restart()
    briefProcess.running = true
    if (!briefProcess.running && !briefProcess.handled) {
      briefDeadline.stop()
      briefProcess.handled = true
      root.recordError("brief", "not installed")
    }
  }

  function pollFull() {
    if (fullProcess.running) return
    fullProcess.timedOut = false
    fullProcess.started = false
    fullProcess.handled = false
    root.setBusy(true)
    fullDeadline.restart()
    fullProcess.running = true
    if (!fullProcess.running && !fullProcess.handled) {
      fullDeadline.stop()
      fullProcess.handled = true
      root.setBusy(false)
      root.recordError("full", "not installed")
    }
  }

  Process {
    id: unitProcess
    property bool timedOut: false
    property bool started: false
    property bool handled: false
    command: ["systemctl", "--user", "show", "llm-index.service", "-p", "ActiveState", "-p", "SubState", "-p", "Result"]
    stdout: StdioCollector { id: unitOut; waitForEnd: true }
    stderr: StdioCollector { id: unitErr; waitForEnd: true }
    onStarted: started = true
    onRunningChanged: {
      if (running) return
      Qt.callLater(function () {
        if (!unitProcess.running && !unitProcess.handled) {
          unitDeadline.stop()
          unitProcess.handled = true
          root.setUnit(Model.unitState(""))
        }
      })
    }
    onExited: function (exitCode, exitStatus) {
      unitDeadline.stop()
      if (unitProcess.handled) return
      unitProcess.handled = true
      if (unitProcess.timedOut || !unitProcess.started || exitCode !== 0 || exitStatus !== 0) {
        root.setUnit(Model.unitState(""))
        return
      }
      root.setUnit(Model.unitState(unitOut.text))
    }
  }

  Timer {
    id: unitDeadline
    interval: 5000
    repeat: false
    onTriggered: {
      if (!unitProcess.running) return
      unitProcess.timedOut = true
      unitProcess.running = false
    }
  }

  Process {
    id: briefProcess
    property bool timedOut: false
    property bool started: false
    property bool handled: false
    command: ["llm-index", "status", "--json", "--brief"]
    stdout: StdioCollector { id: briefOut; waitForEnd: true }
    stderr: StdioCollector { id: briefErr; waitForEnd: true }
    onStarted: started = true
    onRunningChanged: {
      if (running) return
      Qt.callLater(function () {
        if (!briefProcess.running && !briefProcess.handled) {
          briefDeadline.stop()
          briefProcess.handled = true
          root.recordError("brief", briefProcess.timedOut ? "timed out" : (briefProcess.started ? "status failed" : "not installed"))
        }
      })
    }
    onExited: function (exitCode, exitStatus) {
      briefDeadline.stop()
      if (briefProcess.handled) return
      briefProcess.handled = true
      if (briefProcess.timedOut) {
        root.recordError("brief", "timed out")
        return
      }
      if (!briefProcess.started) {
        root.recordError("brief", "not installed")
        return
      }
      if (exitCode !== 0 || exitStatus !== 0) {
        root.recordError("brief", "status failed")
        return
      }
      root.recordSnapshot("brief", briefOut.text)
    }
  }

  Timer {
    id: briefDeadline
    interval: 10000
    repeat: false
    onTriggered: {
      if (!briefProcess.running) return
      briefProcess.timedOut = true
      briefProcess.running = false
    }
  }

  Process {
    id: fullProcess
    property bool timedOut: false
    property bool started: false
    property bool handled: false
    command: ["llm-index", "status", "--json"]
    stdout: StdioCollector { id: fullOut; waitForEnd: true }
    stderr: StdioCollector { id: fullErr; waitForEnd: true }
    onStarted: started = true
    onRunningChanged: {
      root.setBusy(running)
      if (running) return
      Qt.callLater(function () {
        if (!fullProcess.running && !fullProcess.handled) {
          fullDeadline.stop()
          fullProcess.handled = true
          root.recordError("full", fullProcess.timedOut ? "timed out" : (fullProcess.started ? "status failed" : "not installed"))
        }
      })
    }
    onExited: function (exitCode, exitStatus) {
      fullDeadline.stop()
      if (fullProcess.handled) return
      fullProcess.handled = true
      if (fullProcess.timedOut) {
        root.recordError("full", "timed out")
        return
      }
      if (!fullProcess.started) {
        root.recordError("full", "not installed")
        return
      }
      if (exitCode !== 0 || exitStatus !== 0) {
        root.recordError("full", "status failed")
        return
      }
      root.recordSnapshot("full", fullOut.text)
    }
  }

  Timer {
    id: fullDeadline
    interval: 60000
    repeat: false
    onTriggered: {
      if (!fullProcess.running) return
      fullProcess.timedOut = true
      fullProcess.running = false
    }
  }

  Process {
    id: restartProcess
    command: ["systemctl", "--user", "restart", "llm-index.service"]
    stdout: StdioCollector { id: restartOut; waitForEnd: true }
    stderr: StdioCollector { id: restartErr; waitForEnd: true }
    onExited: function () {
      root.now = Math.floor(Date.now() / 1000)
      root.pollUnit()
      root.pollFull()
    }
  }

  Process {
    id: logProcess
    command: ["omarchy-launch-floating-terminal-with-presentation", "journalctl", "--user", "-fu", "llm-index"]
    stdout: StdioCollector { id: logOut; waitForEnd: true }
    stderr: StdioCollector { id: logErr; waitForEnd: true }
  }

  Timer {
    id: unitTimer
    interval: 10000
    running: root.viewers > 0
    repeat: true
    onTriggered: {
      root.now = Math.floor(Date.now() / 1000)
      root.pollUnit()
    }
  }

  Timer {
    id: briefTimer
    interval: root.openPanels > 0 ? 5000 : Model.BRIEF_PERIOD_S * 1000
    running: root.viewers > 0
    repeat: true
    onTriggered: {
      root.now = Math.floor(Date.now() / 1000)
      root.pollBrief()
    }
  }
}
