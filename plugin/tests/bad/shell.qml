// expect: 6
import QtQuick
Item {
  property var a: ["sh", "-c", "true"]
  property var b: ['bash', 'script']
  property var c: ["/bin/sh", "script"]
  property var d: ["/usr/bin/env", "zsh"]
}
