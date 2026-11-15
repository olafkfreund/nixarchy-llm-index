import QtQuick
import qs.Ui
import qs.Commons
import "Model.js" as Model

FocusScope {
  id: root

  property color foreground: Color.foreground
  property string fontFamily: Style.font.family
  readonly property color dim: Qt.darker(foreground, 1.5)
  readonly property Item keyTarget: keyCatcher
  property string currentTab: "status"
  property bool confirmRestartOpen: false

  readonly property var liveRoots: IndexState.live ? IndexState.live.roots : (IndexState.full ? IndexState.full.roots : [])
  readonly property var configErrors: IndexState.live ? IndexState.live.config_errors : (IndexState.full ? [] : [])
  readonly property var databases: IndexState.full ? IndexState.full.databases : []
  readonly property var settingsData: IndexState.full ? IndexState.full.settings : null
  readonly property bool unavailable: !IndexState.live && !IndexState.full
  readonly property string unavailableCategory: IndexState.briefError || IndexState.fullError || ""

  signal closeRequested()
  signal switchPanelRequested(int direction)

  implicitHeight: Math.max(column.implicitHeight, Style.space(220))

  function fullRoot(name) {
    if (!IndexState.full || !IndexState.full.roots) return null
    var list = IndexState.full.roots
    for (var i = 0; i < list.length; i++) {
      if (list[i].name === name) return list[i]
    }
    return null
  }

  function unitSummary() {
    var u = IndexState.unit
    var text = "Unit: " + u.state
    if (u.sub !== "") text += " (" + u.sub + ")"
    if (u.result !== "") text += " · " + u.result
    return text
  }

  function scanAge(lastScan) {
    if (lastScan === null || lastScan === undefined) return Model.fmtAge(null)
    return Model.fmtAge(Math.max(0, IndexState.now - lastScan))
  }

  function countsAge() {
    if (IndexState.fullAt === null || IndexState.fullAt === undefined) return "—"
    return Model.fmtAge(Math.max(0, IndexState.now - IndexState.fullAt))
  }

  function rootCountsLine(liveRow) {
    var fullRow = root.fullRoot(liveRow.name)
    var files = fullRow ? fullRow.files : liveRow.files
    var chunks = fullRow ? fullRow.chunks : liveRow.chunks
    return "files " + Model.fmtCount(files)
      + " · chunks " + Model.fmtCount(chunks)
      + " (counts as of " + root.countsAge() + ")"
  }

  PanelKeyCatcher {
    id: keyCatcher
    anchors.fill: parent
    blocked: restartConfirm.opened
    onCloseRequested: root.closeRequested()
    onTabRequested: function (direction) { root.switchPanelRequested(direction) }

    Column {
      id: column
      width: parent.width
      spacing: Style.spacing.md

      Row {
        width: parent.width
        spacing: Style.spacing.sm

        Button {
          text: "Status"
          selected: root.currentTab === "status"
          bordered: true
          focusable: true
          onClicked: root.currentTab = "status"
        }

        Button {
          text: "Settings"
          selected: root.currentTab === "settings"
          bordered: true
          focusable: true
          onClicked: root.currentTab = "settings"
        }

        Text {
          textFormat: Text.PlainText
          anchors.verticalCenter: parent.verticalCenter
          visible: IndexState.busy
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
          text: "Busy…"
        }
      }

      Column {
        width: parent.width
        spacing: Style.spacing.xs
        visible: root.unavailable

        PanelSectionHeader {
          width: parent.width
          textFormat: Text.PlainText
          text: "Status unavailable"
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          wrapMode: Text.WordWrap
          color: Color.urgent
          font.family: root.fontFamily
          visible: root.unavailableCategory !== ""
          text: root.unavailableCategory
        }
      }

      Column {
        width: parent.width
        spacing: Style.spacing.md
        visible: root.currentTab === "status"

        PanelSectionHeader {
          width: parent.width
          textFormat: Text.PlainText
          text: root.unitSummary()
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          wrapMode: Text.WordWrap
          color: Color.urgent
          font.family: root.fontFamily
          visible: !root.unavailable && (IndexState.briefError !== null || IndexState.fullError !== null)
          text: "Last update error: " + (IndexState.briefError || IndexState.fullError || "")
        }

        Repeater {
          model: root.liveRoots

          Column {
            required property var modelData
            width: column.width
            spacing: Style.spacing.xxs

            Text {
              width: parent.width
              textFormat: Text.PlainText
              wrapMode: Text.WordWrap
              color: (modelData.state === "offline" || modelData.state === "degraded" || modelData.state === "config_error")
                ? Color.urgent : root.foreground
              font.family: root.fontFamily
              text: modelData.name + " [" + modelData.tier + "] · " + modelData.state
                + (modelData.reason ? " (" + modelData.reason + ")" : "")
            }

            Text {
              width: parent.width
              textFormat: Text.PlainText
              wrapMode: Text.WordWrap
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              text: "last scan " + root.scanAge(modelData.last_scan)
                + " · queued " + Model.fmtCount(modelData.queued)
                + " · done " + Model.fmtCount(modelData.done)
            }

            Text {
              width: parent.width
              textFormat: Text.PlainText
              wrapMode: Text.WordWrap
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              text: root.rootCountsLine(modelData)
            }
          }
        }

        Column {
          width: parent.width
          spacing: Style.spacing.xxs
          visible: root.configErrors.length > 0

          PanelSectionHeader {
            width: parent.width
            textFormat: Text.PlainText
            text: "Config errors"
          }

          Repeater {
            model: root.configErrors

            Text {
              required property var modelData
              width: column.width
              textFormat: Text.PlainText
              wrapMode: Text.WordWrap
              color: Color.urgent
              font.family: root.fontFamily
              text: (modelData.root !== null ? modelData.root : "unmatched")
                + " (" + modelData.what + "): "
                + (modelData.reason !== null ? modelData.reason : "other")
            }
          }
        }

        Column {
          width: parent.width
          spacing: Style.spacing.xxs
          visible: root.databases.length > 0

          PanelSectionHeader {
            width: parent.width
            textFormat: Text.PlainText
            text: "Databases"
          }

          Repeater {
            model: root.databases

            Text {
              required property var modelData
              width: column.width
              textFormat: Text.PlainText
              color: root.dim
              font.family: root.fontFamily
              text: modelData.name + ": " + Model.fmtBytes(modelData.bytes)
            }
          }
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          wrapMode: Text.WordWrap
          color: root.dim
          font.family: root.fontFamily
          visible: IndexState.full !== null
          text: "Sandbox (kernel support): " + (IndexState.full ? IndexState.full.sandbox : "—")
        }
      }

      Column {
        width: parent.width
        spacing: Style.spacing.md
        visible: root.currentTab === "settings"

        PanelSectionHeader {
          width: parent.width
          textFormat: Text.PlainText
          text: "Configured roots"
        }

        Repeater {
          model: root.settingsData ? root.settingsData.roots : []

          Text {
            required property var modelData
            width: column.width
            textFormat: Text.PlainText
            wrapMode: Text.WordWrap
            color: root.foreground
            font.family: root.fontFamily
            text: modelData.name + " [" + modelData.tier + "] · "
              + modelData.kind + " · interval " + modelData.interval
              + " · share: " + (modelData.share ? "yes" : "no")
          }
        }

        PanelSectionHeader {
          width: parent.width
          textFormat: Text.PlainText
          visible: root.settingsData !== null
          text: "Exclusions & limits"
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          wrapMode: Text.WordWrap
          color: root.dim
          font.family: root.fontFamily
          visible: root.settingsData !== null
          text: root.settingsData
            ? ("exclusions: " + Model.fmtCount(root.settingsData.exclusions)
               + " · lifted builtin: " + Model.fmtCount(root.settingsData.lifted_builtin))
            : ""
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          wrapMode: Text.WordWrap
          color: root.dim
          font.family: root.fontFamily
          visible: root.settingsData !== null
          text: root.settingsData
            ? ("workers: " + Model.fmtCount(root.settingsData.limits.workers)
               + " · memory max: " + Model.fmtCount(root.settingsData.limits.memory_max_mb) + " MB"
               + " · max file: " + Model.fmtCount(root.settingsData.limits.max_file_mb) + " MB"
               + " · max text: " + Model.fmtBytes(root.settingsData.limits.max_text_bytes)
               + " · max cells: " + Model.fmtCount(root.settingsData.limits.max_cells)
               + " · backoff max: " + root.settingsData.limits.backoff_max)
            : ""
        }

        Text {
          width: parent.width
          textFormat: Text.PlainText
          wrapMode: Text.WordWrap
          color: root.dim
          font.family: root.fontFamily
          text: "Managed by Nix: `services.llm-index` in Home Manager. Change it in your system configuration and rebuild."
        }
      }

      Row {
        spacing: Style.spacing.sm

        Button {
          text: "Refresh"
          bordered: true
          focusable: true
          onClicked: IndexState.refresh()
        }

        Button {
          text: "Restart"
          bordered: true
          focusable: true
          onClicked: root.confirmRestartOpen = true
        }

        Button {
          text: "Open log"
          bordered: true
          focusable: true
          onClicked: IndexState.openLog()
        }
      }
    }

    ConfirmDialog {
      id: restartConfirm
      anchors.fill: parent
      z: 20
      opened: root.confirmRestartOpen
      message: "Restart llm-index.service?"
      confirmText: "Restart"
      foreground: root.foreground
      fontFamily: root.fontFamily
      onOpenedChanged: {
        selectedIndex = 1
        if (opened) restartConfirm.forceActiveFocus()
        else keyCatcher.forceActiveFocus()
      }
      Keys.onPressed: function (event) {
        if (restartConfirm.handleKey(event)) event.accepted = true
      }
      onCanceled: root.confirmRestartOpen = false
      onConfirmed: {
        root.confirmRestartOpen = false
        IndexState.restartService()
      }
    }
  }
}
