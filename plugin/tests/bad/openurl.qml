// expect: 2
import QtQuick
Item {
  Component.onCompleted: Qt.openUrlExternally("https://example.org")
  function f(u) { Qt.openUrlExternally(u) }
}
