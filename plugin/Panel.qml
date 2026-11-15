import QtQuick
import Quickshell
import Quickshell.Io
import qs.Ui
import qs.Commons
import qs.Commons as Commons
import "Model.js" as Model

Panel {
  id: root

  moduleName: "olafkfreund.llm-index"
  ipcTarget: "olafkfreund.llm-index.bar"
  manageIpc: false

  readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  property bool panelTracked: false

  Component.onCompleted: IndexState.acquire()
  Component.onDestruction: {
    if (root.panelTracked) {
      root.panelTracked = false
      IndexState.panelClosed()
    }
    IndexState.release()
  }

  onOpenedChanged: function () {
    if (root.opened && !root.panelTracked) {
      root.panelTracked = true
      IndexState.panelOpened()
    } else if (!root.opened && root.panelTracked) {
      root.panelTracked = false
      IndexState.panelClosed()
    }
  }

  IpcHandler {
    target: root.ipcTarget

    function open(): void { root.open() }
    function close(): void { root.close() }
    function toggle(): void { root.toggle() }
    function status(): string {
      return JSON.stringify({ level: IndexState.level })
    }
  }

  implicitWidth: button.visible ? button.implicitWidth : 0
  implicitHeight: button.implicitHeight

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "\uf1c0"
    visible: IndexState.level !== "ok" || root.opened || IndexState.openPanels > 0
    active: IndexState.level === "down" || IndexState.level === "problem" || IndexState.level === "indexing"
    useActiveColor: true
    activeColor: (IndexState.level === "down" || IndexState.level === "problem") ? Commons.Color.urgent : Commons.Color.accent
    tooltipText: Model.tooltip(IndexState.level)
    onPressed: function (b) { root.toggle() }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: view.keyTarget
    contentWidth: panel.fittedContentWidth(Style.space(520))
    contentHeight: panel.fittedContentHeight(view.implicitHeight)

    IndexView {
      id: view
      anchors.fill: parent
      foreground: root.foreground
      fontFamily: root.fontFamily
      onCloseRequested: root.close()
      onSwitchPanelRequested: function (direction) { root.switchPanel(direction) }
    }
  }
}
